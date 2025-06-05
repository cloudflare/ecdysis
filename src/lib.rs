#![doc = include_str!("../README.md")]

#[cfg(feature = "tokio_ecdysis")]
pub mod tokio_ecdysis;

mod executioner;
mod inheriter;
mod listener;
mod registry;
#[cfg(target_os = "linux")]
#[cfg(feature = "tokio_ecdysis")]
mod seqpacket;
mod utils;

use std::{
    io,
    io::Write,
    net::{SocketAddr, TcpListener, UdpSocket},
    os::unix::{
        io::{AsRawFd, FromRawFd},
        net::{UnixDatagram, UnixListener},
    },
    path::{Path, PathBuf},
    process::exit,
};

use socket2::Socket;
#[cfg(target_os = "linux")]
#[cfg(feature = "tokio_ecdysis")]
pub use tokio_seqpacket::UnixSeqpacketListener;

use executioner::{upgrade, UpgradeFinished};
use inheriter::{init_child, InheritError};
use registry::{ListenerRegistry, SockInfo};

// reexports
pub use crate::{listener::Listener, registry::ListenerInfo};
#[cfg(feature = "systemd_notify")]
pub use tokio_ecdysis::systemd_notify::{SystemdNotifier, SystemdNotifierError};

#[cfg(feature = "systemd_sockets")]
pub use tokio_ecdysis::systemd_sockets::{SystemdSocketError, SystemdSocketsReadError};

/// Ecdysis - upgrade manager and entry point for graceful upgrades
///
/// Ecdysis manages upgrades by:
/// 1. Managing listening sockets
/// 2. Handling the process fork/exec cycle in a correct and recoverable way.
///
/// The combined result of these behaviors is that:
/// * During an upgrade, managed sockets will continue to accept new connections.
/// * Errors in the child process will not interrupt anything using the sockets.
/// * After a child indicates readiness it will begin accepting connections, and the parent will
///   stop accepting connections.
/// * The parent can wait for existing connections to finish (drain) and exit with no premature
///   connection close.
///
/// DOC TODO: Example
pub struct Ecdysis {
    registry: ListenerRegistry,
    ready_notifier: Option<os_pipe::PipeWriter>,
    pid_file: Option<PathBuf>,
    child: bool,
}

impl Default for Ecdysis {
    fn default() -> Self {
        Self::new()
    }
}

impl Ecdysis {
    pub fn new() -> Self {
        let registry: ListenerRegistry;
        let ready_notifier: Option<os_pipe::PipeWriter>;
        let child: bool;

        match init_child() {
            Ok((fds, ready_pipe)) => {
                registry = ListenerRegistry::from_inherited(fds);
                ready_notifier = Some(ready_pipe);
                child = true;
            }
            Err(InheritError::NotAnUpgrade) => {
                registry = ListenerRegistry::new();
                ready_notifier = None;
                child = false;
            }
            Err(e) => {
                log::error!("Fatal: Upgrade environment problem - {e}");
                exit(1)
            }
        };
        Self {
            registry,
            ready_notifier,
            child,
            pid_file: None,
        }
    }

    /// If this process is the result of calling ecdysis.upgrade(), this will return true
    pub fn is_child(&self) -> bool {
        self.child
    }

    /// Set a PID file for this application. A PID file allows proper pid tracking across the
    /// fork/exec when used with process supervisors (e.g. systemd).
    ///
    /// If a PIDFile has been set, the `ready` method will:
    ///   1: create a temporary file
    ///   2: write the pid of the process to it.
    ///   3: move that file to the actual pid file location.
    /// The parent process will always rewrite the PID file to contain it's PID if it returns an
    /// error from the `upgrade` method.
    pub fn set_pid_file<P: AsRef<Path>>(&mut self, pid_file: P) {
        self.pid_file = Some(PathBuf::from(pid_file.as_ref()))
    }

    fn write_pidfile(&self) -> io::Result<()> {
        match &self.pid_file {
            Some(pid_file) => utils::write_pid_file(pid_file),
            None => Ok(()),
        }
    }

    /// Signal to parent that the child process is properly set up and ready to take over. If this
    /// is not called, the parent will continue to listen and accept connections on the listening
    /// sockets. The child will also be able to start accepting connections. If ready is not called
    /// in the child, the parent will eventually timeout and kill the child on the assumption that
    /// something is broken in the child.
    pub fn ready(&mut self) -> io::Result<()> {
        self.registry.close_inherited();

        let _ = self.write_pidfile();

        if let Some(mut notifier) = self.ready_notifier.take() {
            notifier.write_all(b"OK")
        } else {
            Ok(())
        }
    }

    /// Begin the upgrade procedure. The process that calls upgrade becomes the parent, and sets
    /// up, then executes the fork/exec to start the child process.
    pub fn upgrade(&self) -> UpgradeFinished {
        let fds = self.registry.get_fds_for_child();
        log::warn!("Ecdysis starting upgrade");
        upgrade(fds).map_err(|e| {
            log::warn!("Upgrade failed! - {e}");
            let _ = self.write_pidfile();
            e
        })
    }

    /// Call this function when this process is shutting down instead of upgrading. Ecdysis will
    /// close and forget about all FDs that it contains. Note that the application _also_ needs to
    /// close it's clone of each listening FD in order for the system to actually stop listening
    /// for incoming connections.
    pub fn quit(&self) {
        self.registry.close_used()
    }

    /// Create a registered UnixListener bound to `path`. In an upgrade, this will return a
    /// UnixListener bound to the same socket as in the parent. If the parent wasn't using this
    /// socket, or this isn't an upgrade, a new socket is created for the listner. This socket can
    /// then be used in an accept loop (or via the `listener.incoming()` iterator).
    ///
    /// TODO: do we want to handle socket deletion here or leave it the caller's responsiblility
    ///       (follow on: if so, should we handle "delete on close" semantics?)
    ///
    pub fn listen_unix<P>(&self, path: P) -> io::Result<UnixListener>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        log::debug!("Creating unix listener at: {:?}", path);
        let listener = match self
            .registry
            .inherit(SockInfo::Unix(Some(path.as_ref().into())))
        {
            Some(fd) => {
                log::debug!("Found existing fd, opening");
                unsafe { UnixListener::from_raw_fd(fd) }
            }
            None => {
                log::debug!("Does not exist, creating new");
                let listener = UnixListener::bind(path)?;
                self.registry.add(listener.info()?)?;
                listener
            }
        };

        Ok(listener)
    }

    #[cfg(target_os = "linux")]
    #[cfg(feature = "tokio_ecdysis")]
    fn listen_unix_seqpacket<P>(&self, path: P) -> io::Result<UnixSeqpacketListener>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        log::debug!("Creating unix seqpacket listener at: {:?}", path);
        let listener = match self
            .registry
            .inherit(SockInfo::UnixSeqpacket(Some(path.as_ref().into())))
        {
            Some(fd) => {
                log::debug!("Found existing fd, opening");
                unsafe { UnixSeqpacketListener::from_raw_fd(fd) }?
            }
            None => {
                log::debug!("Does not exist, creating new");
                let listener = UnixSeqpacketListener::bind(path)?;
                self.registry.add(listener.info()?)?;
                listener
            }
        };

        Ok(listener)
    }

    /// Create a registered TCP Socket bound to `addr`. In an upgrade, this will return a
    /// UnixListener bound to the same socket as in the parent. If the parent wasn't using this
    /// socket, or this isn't an upgrade, a new socket is created for the listner. This socket can
    /// then be used in an accept loop (or via the `listener.incoming()` iterator).
    pub fn listen_tcp(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        self.build_listen_tcp(addr, |b, addr| {
            b.bind(&addr.into())?;
            b.listen(128)?;
            Ok(b.into())
        })
    }

    /// Create a TcpListener bound to `addr`. The closure `sock_build` must accept a
    /// socket2::Socket and return a configured TcpListener. In an upgrade, this will return a
    /// TcpListener bound to the same socket as in the parent. If the parent wasn't using this
    /// socket, or this isn't an upgrade, a new socket using the `sock_build` closure. This closure
    /// is only called when the socket is being set up for the first time - it's ignored in the
    /// upgrade case where we have a file descriptor present from the parent.
    pub fn build_listen_tcp<F>(&self, addr: SocketAddr, sock_build: F) -> io::Result<TcpListener>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<TcpListener>,
    {
        log::debug!("Creating TCP listener on: {:?}", addr);
        let listener = match self.registry.inherit(SockInfo::Tcp(addr)) {
            Some(fd) => {
                log::debug!("Found existing TCP fd, opening");
                unsafe { TcpListener::from_raw_fd(fd) }
            }
            None => {
                log::debug!("TCP fd does not exist, creating new");
                let builder = Socket::new(
                    socket2::Domain::for_address(addr),
                    socket2::Type::STREAM,
                    None,
                )?;
                let listener = sock_build(builder, addr)?;
                self.registry.add(listener.info()?)?;
                listener
            }
        };

        log::debug!(
            "set up listener, now have registry of:\n {:?}",
            self.registry.get_fds_for_child()
        );
        Ok(listener)
    }

    /// Create a UdpSocket bound to `addr`. The closure `sock_build` must accept a
    /// socket2::Socket and return a configured UdpSocket. In an upgrade, this will return a
    /// UdpSocket bound to the same socket as in the parent. If the parent wasn't using this
    /// socket, or this isn't an upgrade, a new socket using the `sock_build` closure. This closure
    /// is only called when the socket is being set up for the first time - it's ignored in the
    /// upgrade case where we have a file descriptor present from the parent.
    pub fn build_socket_udp<F>(&self, addr: SocketAddr, sock_build: F) -> io::Result<UdpSocket>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<UdpSocket>,
    {
        log::debug!("Creating UDP socket on: {:?}", addr);
        let socket = match self.registry.inherit(SockInfo::Udp(addr)) {
            Some(fd) => {
                log::debug!("Found existing UDP fd, opening");
                unsafe { UdpSocket::from_raw_fd(fd) }
            }
            None => {
                log::debug!("UDP fd does not exist, creating new");
                let builder = Socket::new(
                    socket2::Domain::for_address(addr),
                    socket2::Type::DGRAM,
                    None,
                )?;
                let socket = sock_build(builder, addr)?;
                self.registry.add(socket.info()?)?;
                socket
            }
        };

        log::debug!(
            "set up socket, now have registry of:\n {:?}",
            self.registry.get_fds_for_child()
        );
        Ok(socket)
    }

    /// This can be used to form a chain of `UnixDatagram` pairs between successive upgrades. The
    /// first `UnixDatagram` is one that the parent process has the other end to (of the same given
    /// name), if this is an upgrade and the parent process created a `UnixDatagram` with the same
    /// name. The other end the second `UnixDatagram` will be passed to the child of this process
    /// (in the case of failed upgrades, note that the same `UnixDatagram` instance will be passed
    /// to all children).
    pub fn unix_datagram_pair(
        &self,
        name: String,
    ) -> (Option<UnixDatagram>, io::Result<UnixDatagram>) {
        let sock_info = SockInfo::UnboundUnixDatagram(name);

        let unix_datagram_to_parent_option = self
            .registry
            .take(&sock_info)
            .map(|fd| unsafe { UnixDatagram::from_raw_fd(fd) });

        let unix_datagram_to_child_result =
            UnixDatagram::pair().and_then(|(unix_datagram_to_child, childs_unix_datagram)| {
                self.registry.add(ListenerInfo {
                    fd: childs_unix_datagram.as_raw_fd(),
                    sock_info,
                })?;

                Ok(unix_datagram_to_child)
            });

        (
            unix_datagram_to_parent_option,
            unix_datagram_to_child_result,
        )
    }
}
