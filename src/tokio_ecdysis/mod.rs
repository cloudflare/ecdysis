//! Tokio wrappers for Ecdysis

#[cfg(feature = "systemd_notify")]
use std::time::Duration;
use std::{
    future::Future,
    io, mem,
    net::SocketAddr,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    task::{Context, Poll},
    thread,
};

#[cfg(feature = "systemd_sockets")]
pub(super) mod systemd_sockets;

#[cfg(feature = "systemd_sockets")]
use {
    crate::registry::SockInfo,
    std::os::fd::FromRawFd,
    systemd_sockets::{SystemdSocketError, SystemdSockets, SystemdSocketsReadError},
};

use bytes::Bytes;
use futures::{stream::SplitStream, StreamExt};
use parking_lot::RwLock;
use socket2::Socket;
use tokio::{
    net::{TcpListener, UdpSocket, UnixDatagram, UnixListener},
    signal::unix::signal,
    sync::oneshot::channel,
};
use tokio_stream::wrappers::{SignalStream, TcpListenerStream, UnixListenerStream};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

#[cfg(target_os = "linux")]
use crate::seqpacket::UnixSeqpacketListenerStream;

use super::{executioner::upgrade, Ecdysis, UpgradeFinished};

use supervisor::Supervisor;
#[cfg(feature = "systemd_notify")]
use systemd_notify::{SystemdNotifier, SystemdNotifierError};
use trigger::{Trigger, TriggerReason};

pub mod supervisor;
#[cfg(feature = "systemd_notify")]
pub(super) mod systemd_notify;
mod trigger;

pub type UdpStream = SplitStream<UdpFramed<BytesCodec>>;

// re-export
pub use supervisor::{StopOnShutdown, Stoppable, StoppableStream};
pub use tokio::signal::unix::SignalKind;

/// [`ExitMode`] represents the mode of successful Ecdysis shutdown. Useful for (e.g.) knowing when
/// to clean up tempfiles or unix domain socket paths.
#[derive(Debug)]
pub enum ExitMode {
    Upgrade,
    FullStop,
    PartialStop,
    #[cfg(feature = "systemd_notify")]
    /// [`ExitMode::PartialStopWithSystemd`] is a special variant only returned by the
    /// [`TokioEcdysis`] future when a partial shutdown is triggered when systemd-notify integration
    /// is enabled and active. See [`TokioEcdysisBuilder::partial_stop_on_signal`] for more details.
    PartialStopWithSystemd(SystemdNotifier),
}

/// Holds relevant information relating to the reason why an
/// Ecdysis shutdown or upgrade was initiated.
#[derive(Debug, Clone)]
pub enum ExitReason {
    Signal(SignalKind),
    UnixListener(PathBuf),
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum ExitCondition {
    Upgrade,
    Stop,
    PartialStop,
}

pub struct TokioEcdysisBuilder {
    tokio_ecdysis: TokioEcdysis,
    triggers: Vec<(Trigger, ExitCondition)>,
    #[cfg(feature = "systemd_notify")]
    systemd_notifier: Option<SystemdNotifier>,
}

/// Allows methods to be called on TokioEcdysis in order to build and inherit sockets before
/// ready() is called.
impl Deref for TokioEcdysisBuilder {
    type Target = TokioEcdysis;

    fn deref(&self) -> &Self::Target {
        &self.tokio_ecdysis
    }
}

impl TokioEcdysisBuilder {
    pub fn new(upgrade_signal_kind: SignalKind) -> io::Result<Self> {
        let triggers: Vec<(Trigger, ExitCondition)> = vec![(
            Trigger::Signal(
                upgrade_signal_kind,
                SignalStream::new(signal(upgrade_signal_kind)?),
            ),
            ExitCondition::Upgrade,
        )];

        Ok(Self {
            tokio_ecdysis: TokioEcdysis::new(),
            triggers,
            #[cfg(feature = "systemd_notify")]
            systemd_notifier: None,
        })
    }

    fn trigger_on_signal(
        &mut self,
        signal_kind: SignalKind,
        trigger_action: ExitCondition,
    ) -> io::Result<()> {
        self.triggers.push((
            Trigger::Signal(signal_kind, SignalStream::new(signal(signal_kind)?)),
            trigger_action,
        ));
        Ok(())
    }

    /// Set TokioEcdysis to cleanly shutdown the process when it receives exit_signal. This will
    /// cause a signal handler to be registered for exit_signal along with the upgrade_signal
    /// provided at construction time.  The `exit_signal` should not be the same as
    /// `upgrade_signal` - if they are the same exit signal will result in an upgrade. This exits
    /// cleanly the same a after a successful upgrade.
    /// Multiple, distinct exit signals can be registered through repeated calls to this method.
    /// If the signal handlers have already been initialized as a result of giving TokioEcdysis
    /// to the tokio reactor as a future, this will return an Err()
    pub fn stop_on_signal(&mut self, signal_kind: SignalKind) -> io::Result<()> {
        self.trigger_on_signal(signal_kind, ExitCondition::Stop)
    }

    /// Registers a signal to trigger a partial shutdown of Ecdysis. When a partial shutdown is
    /// triggered, Ecdysis will stop monitoring all triggers, but will skip all other shutdown
    /// steps. In particular, during a partial shutdown, if systemd-notify integration is enabled,
    /// Ecdysis will _not_ notify systemd of an impending shutdown; responsibility for doing that
    /// passes on to the caller. To make this easier, on a partial shutdown when systemd-notify
    /// integration is enabled, the [`TokioEcdysis`] future will return an
    /// [`ExitMode`] variant containing the systemd-notify client.
    pub fn partial_stop_on_signal(&mut self, signal_kind: SignalKind) -> io::Result<()> {
        self.trigger_on_signal(signal_kind, ExitCondition::PartialStop)
    }

    fn trigger_on_socket<P>(
        &mut self,
        listen_path: P,
        trigger_action: ExitCondition,
    ) -> io::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        self.triggers.push((
            Trigger::Uds(
                listen_path.as_ref().to_path_buf(),
                self.tokio_ecdysis
                    .listen_unix(StopOnShutdown::Yes, listen_path)?,
            ),
            trigger_action,
        ));
        Ok(())
    }

    /// Set TokioEcdysis to listen on an Unix Domain Socket at `listen_path` and trigger an upgrade
    /// when a connection is made to this socket. This connection remains open until the upgrade has
    /// been completed.
    pub fn upgrade_on_socket<P>(&mut self, listen_path: P) -> io::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        self.trigger_on_socket(listen_path, ExitCondition::Upgrade)
    }

    /// Set TokioEcdysis to listen on an Unix Domain Socket at `listen_path` and trigger a graceful
    /// shutdown when a connection is made to this socket. This connection remains open until the
    /// shutdown has been completed.
    pub fn stop_on_socket<P>(&mut self, listen_path: P) -> io::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        self.trigger_on_socket(listen_path, ExitCondition::Stop)
    }

    /// Registers a UDS to trigger a partial shutdown of Ecdysis. When a partial shutdown is
    /// triggered, Ecdysis will stop monitoring all triggers, but will skip all other shutdown
    /// steps. In particular, during a partial shutdown, if systemd-notify integration is enabled,
    /// Ecdysis will _not_ notify systemd of an impending shutdown; responsibility for doing that
    /// passes on to the caller. To make this easier, on a partial shutdown when systemd-notify
    /// integration is enabled, the [`TokioEcdysis`] future will return an
    /// [`ExitMode`] variant containing the systemd-notify client.
    pub fn partial_stop_on_socket<P>(&mut self, listen_path: P) -> io::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        self.trigger_on_socket(listen_path, ExitCondition::PartialStop)
    }

    /// Set a pidfile for this application. See Ecdysis::set_pid_file()
    pub fn set_pid_file<P: AsRef<Path>>(&mut self, pid_file: P) {
        self.tokio_ecdysis.inner.set_pid_file(pid_file)
    }

    #[cfg(feature = "systemd_notify")]
    pub fn enable_systemd_notifications(&mut self) -> Result<(), SystemdNotifierError> {
        // Make sure to verify that we can notify systemd _before_ notifying our parent that we are
        // ready, to avoid situations in which the parent thinks the child is ready but the child
        // crashes silently because it cannot communicate with systemd.
        self.systemd_notifier = Some(SystemdNotifier::new()?);

        Ok(())
    }

    /// Notifies systemd to extend the startup, runtime, and shutdown timeouts for this process.
    /// Useful, for instance, in cases where shutdown after Ecdysis exit is not immediate.
    /// Has no effect if systemd notifications are not enabled.
    #[cfg(feature = "systemd_notify")]
    pub async fn extend_systemd_timeouts(&mut self, extension: Duration) -> io::Result<()> {
        if let Some(systemd_notifier) = &mut self.systemd_notifier {
            systemd_notifier.notify_extend_timeouts(extension).await?;
        }

        Ok(())
    }

    /// Consume this builder and signal readiness to parent. Returns an `Arc<TokioEcdysis>`, with
    /// which further sockets may optionally be created, and also returns with a Future to await
    /// for the upgrade or shutdown.
    pub fn ready(
        self,
    ) -> io::Result<(
        Arc<TokioEcdysis>,
        impl Future<Output = TokioEcdysisUpgradeResult>,
    )> {
        let Self {
            mut tokio_ecdysis,
            triggers,
            #[cfg(feature = "systemd_notify")]
            systemd_notifier,
        } = self;

        tokio_ecdysis.inner.ready()?;

        let tokio_ecdysis_arc = Arc::new(tokio_ecdysis);

        let upgrader = TokioEcdysisUpgrader {
            tokio_ecdysis: tokio_ecdysis_arc.clone(),
            triggers,
            #[cfg(feature = "systemd_notify")]
            systemd_notifier,
        };

        Ok((tokio_ecdysis_arc, upgrader.monitor()))
    }

    /// read_systemd_sockets will parse environment variables set by systemd to find sockets.
    ///
    /// See [`TokioEcdysis::read_systemd_sockets`] for more information.
    #[cfg(feature = "systemd_sockets")]
    pub fn read_systemd_sockets(&mut self) -> Result<(), SystemdSocketsReadError> {
        self.tokio_ecdysis.read_systemd_sockets()
    }
}

/// Tokio-ready ecdysis wrapper
///
/// This both wraps [`Ecdysis`] and provides a bit of extra functionality that is non-obvious to
/// implement in tokio. [TokioEcdysis] is a future, that will run waiting for a unix signal (set at
/// initialization). Once that signal is received, it will begin the upgrade process and on upgrade
/// success stop all registered listeners, and wait for ongoing-connections to drain, before
/// returning from the tokio runtime.
///
/// Optionally TokioEcdysis can listen for a second unix signal which will gracefully shut the
/// ecdysis down (following the same steps described above, just without the fork-exec to spin up
/// a child instnace) - see `exit_on_signal`.
///
/// The Streams returned by `listen_*` and `build_listen_tcp` are instances of [`StoppableStream`]
/// which is a thin wrapper around [`TcpListener`] and [`UnixListener`] that allows stream of new
/// connections to be stopped by the supervisor, closing the listening socket. Ongoing connections
/// that were already established are maintained until they exit naturally.
pub struct TokioEcdysis {
    supervisor: RwLock<Supervisor>,
    inner: Ecdysis,
    #[cfg(feature = "systemd_sockets")]
    systemd_sockets: Option<SystemdSockets>,
}

impl TokioEcdysis {
    fn new() -> Self {
        Self {
            supervisor: RwLock::new(Supervisor::new()),
            inner: Ecdysis::new(),
            #[cfg(feature = "systemd_sockets")]
            systemd_sockets: None,
        }
    }

    /// Determine if this is the first run of the program, or an upgraded child.
    pub fn is_child(&self) -> bool {
        self.inner.is_child()
    }

    /// Can be used to construct standard (blocking) sockets
    pub fn std_ecdysis(&self) -> &Ecdysis {
        &self.inner
    }

    /// Listen on a unix domain socket. Returns a [`StoppableStream`'] ready for the tokio reactor
    pub fn listen_unix<P>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        path: P,
    ) -> io::Result<StoppableStream<UnixListenerStream>>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let listener = self.inner.listen_unix(path)?;
        let listener = UnixListener::from_std(listener)?;
        let listener = UnixListenerStream::new(listener);
        Ok(self
            .supervisor
            .read()
            .supervise_stream(listener, stop_on_shutdown))
    }

    /// Listen on a unix seqpacket socket. Returns a [`StoppableStream`] ready for the tokio reactor
    #[cfg(target_os = "linux")]
    pub fn listen_unix_seqpacket<P>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        path: P,
    ) -> io::Result<StoppableStream<UnixSeqpacketListenerStream>>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let listener = self.inner.listen_unix_seqpacket(path)?;
        let listener = UnixSeqpacketListenerStream::new(listener);
        Ok(self
            .supervisor
            .read()
            .supervise_stream(listener, stop_on_shutdown))
    }

    /// Listen on a TCP socket. Returns a StoppableStream ready for the tokio reactor
    pub fn listen_tcp(
        &self,
        stop_on_shutdown: StopOnShutdown,
        addr: SocketAddr,
    ) -> io::Result<StoppableStream<TcpListenerStream>> {
        self.build_listen_tcp(stop_on_shutdown, addr, |b, addr| {
            b.bind(&addr.into())?;
            b.listen(128)?;
            Ok(b.into())
        })
    }

    /// Listen on a TCP socket. Returns a [`StoppableStream`] ready for the tokio reactor. See
    /// [`Ecdysis::build_listen_tcp()`] for details about the closure argument.
    pub fn build_listen_tcp<F>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        addr: SocketAddr,
        sock_build: F,
    ) -> io::Result<StoppableStream<TcpListenerStream>>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<std::net::TcpListener>,
    {
        let listener = self.inner.build_listen_tcp(addr, sock_build)?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let listener = TcpListenerStream::new(listener);
        Ok(self
            .supervisor
            .read()
            .supervise_stream(listener, stop_on_shutdown))
    }

    /// Listen on a UDP socket. Returns a [`StoppableStream`] ready for the tokio reactor. See
    /// [`Ecdysis::build_socket_udp`] for details about the closure argument.
    pub fn build_stream_udp<F>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        addr: SocketAddr,
        sock_build: F,
    ) -> io::Result<StoppableStream<UdpStream>>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<std::net::UdpSocket>,
    {
        let socket = self.build_socket_udp(addr, sock_build)?;
        let (_s, reader) = UdpFramed::new(socket, BytesCodec::new()).split::<(Bytes, _)>();
        Ok(self
            .supervisor
            .read()
            .supervise_stream(reader, stop_on_shutdown))
    }

    /// Listen on a UDP socket. Returns [`Stoppable<UdpSocket>`]. See
    pub fn build_stoppable_socket_udp<F>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        addr: SocketAddr,
        sock_build: F,
    ) -> io::Result<Stoppable<UdpSocket>>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<std::net::UdpSocket>,
    {
        let socket = self.build_socket_udp(addr, sock_build)?;
        Ok(self.supervisor.read().supervise(socket, stop_on_shutdown))
    }

    fn build_socket_udp<F>(&self, addr: SocketAddr, sock_build: F) -> io::Result<UdpSocket>
    where
        F: FnOnce(Socket, SocketAddr) -> io::Result<std::net::UdpSocket>,
    {
        let socket = self.inner.build_socket_udp(addr, sock_build)?;
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket)
    }

    /// This can be used to form a chain of `UnixDatagram` pairs between successive upgrades. See
    /// [`Ecdysis::unix_datagram_pair()`] for details.
    pub fn unix_datagram_pair(
        &self,
        name: String,
    ) -> (Option<io::Result<UnixDatagram>>, io::Result<UnixDatagram>) {
        let (unix_datagram_to_parent_option, unix_datagram_to_child_result) =
            self.inner.unix_datagram_pair(name);

        let from_std = |s: std::os::unix::net::UnixDatagram| {
            s.set_nonblocking(true)?;
            UnixDatagram::from_std(s)
        };

        (
            unix_datagram_to_parent_option.map(from_std),
            unix_datagram_to_child_result.and_then(from_std),
        )
    }

    /// read_systemd_sockets will parse environment variables set by systemd to find sockets.
    ///
    /// This must be called before any systemd_listen_* calls to receive sockets.
    /// This function cannot be called more than once.
    #[cfg(feature = "systemd_sockets")]
    pub fn read_systemd_sockets(&mut self) -> Result<(), SystemdSocketsReadError> {
        if self.is_child() {
            return Ok(());
        }
        if self.systemd_sockets.is_some() {
            return Err(SystemdSocketsReadError::DuplicateSystemdSocketsRead);
        }

        let systemd_sockets = SystemdSockets::new()?;
        self.systemd_sockets = Some(systemd_sockets);
        Ok(())
    }

    /// systemd_sock_of_proto is a helper function that can be called to find a Unix/TCP/UDP systemd
    /// socket.
    ///
    /// If the current instance is a child, the `sock_info` is used to find the socket in the
    /// registry. If the socket is not found, an error is returned
    ///
    /// If the current instance is the parent, i.e. the first process that was started by systemd,
    /// then find the socket in systemd_sockets, using `name`. If found, the `sock_info` arg is
    /// checked against the socket's actual SockInfo.
    /// Finally the socket is added into the registry, so child instances can inherit the socket.
    ///
    #[cfg(feature = "systemd_sockets")]
    async fn systemd_sock_of_proto<P>(
        &self,
        name: String,
        sock_info: SockInfo,
    ) -> Result<P, SystemdSocketError>
    where
        P: FromRawFd + crate::listener::Listener,
    {
        if self.is_child() {
            return self.read_sock_from_registry_of_proto(sock_info);
        }

        log::debug!(
            "parent: find systemd sock with name {:?} in systemd_sockets",
            name
        );
        let listener = match &self.systemd_sockets {
            None => {
                // systemd_sockets not initialized. read_systemd_sockets was not called or failed
                return Err(SystemdSocketError::SystemdSocketsNotInitialized);
            }
            Some(s) => {
                let fd = s.find(name).await?;
                unsafe { P::from_raw_fd(fd) }
            }
        };

        let listener_info = listener.info()?;
        if listener_info.sock_info != sock_info {
            return Err(SystemdSocketError::SockInfoIncorrect(format!(
                "systemd sock info {:?} does not match expected sock info {:?}",
                listener_info.sock_info, sock_info
            )));
        }

        self.inner.registry.add(listener.info()?)?;
        Ok(listener)
    }

    #[cfg(feature = "systemd_sockets")]
    fn read_sock_from_registry_of_proto<P>(
        &self,
        sock_info: SockInfo,
    ) -> Result<P, SystemdSocketError>
    where
        P: FromRawFd + crate::listener::Listener,
    {
        log::debug!(
            "child: find systemd sock with SockInfo {:?} in registry",
            sock_info
        );
        debug_assert!(self.is_child());

        match self.inner.registry.inherit(sock_info.clone()) {
            Some(fd) => {
                log::debug!("Found existing fd in registry");
                let sock = unsafe { P::from_raw_fd(fd) };
                Ok(sock)
            }
            None => {
                log::debug!("fd does not exist");
                Err(SystemdSocketError::SocketNotFoundInChildRegistry(format!(
                    "socket not found with SockInfo {:?}",
                    sock_info
                )))
            }
        }
    }

    /// Find the Unix listen with `name` from the sockets passed from systemd.
    /// The function will check that the unix socket is bound to `path`.
    ///
    /// **Unlike other listen_ functions, this function will never create the socket.**
    ///
    /// The function behaves differently based on the instance.
    ///
    /// In the parent instance, i.e. this is the process that was lauched for the first time by
    /// systemd, then this function will read the environment variables set by systemd to receive
    /// the socket and add it to the registry. Sockets in the registry will be passed to a child
    /// instance, when a restart signal is received.
    ///
    /// After an upgrade, in child instances, this function will search the sockets passed from the
    /// parent to find the socket.
    ///
    #[cfg(feature = "systemd_sockets")]
    pub async fn systemd_listen_unix<P>(
        &self,
        stop_on_shutdown: StopOnShutdown,
        name: String,
        path: P,
    ) -> Result<StoppableStream<UnixListenerStream>, SystemdSocketError>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let std_listener: std::os::unix::net::UnixListener = self
            .systemd_sock_of_proto(name, SockInfo::Unix(Some(path.as_ref().into())))
            .await?;
        std_listener.set_nonblocking(true)?;
        let tokio_listener = UnixListener::from_std(std_listener)?;
        let listener = UnixListenerStream::new(tokio_listener);
        Ok(self
            .supervisor
            .read()
            .supervise_stream(listener, stop_on_shutdown))
    }

    /// Find the TCP listen socket with `addr` from the sockets passed from systemd.
    /// The function will check if the found socket is bound to `addr`.
    ///
    /// See [`TokioEcdysis::systemd_listen_unix`] for more information.
    #[cfg(feature = "systemd_sockets")]
    pub async fn systemd_listen_tcp(
        &self,
        stop_on_shutdown: StopOnShutdown,
        name: String,
        addr: SocketAddr,
    ) -> Result<StoppableStream<TcpListenerStream>, SystemdSocketError> {
        let listener: std::net::TcpListener = self
            .systemd_sock_of_proto(name, SockInfo::Tcp(addr))
            .await?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let listener = TcpListenerStream::new(listener);
        Ok(self
            .supervisor
            .read()
            .supervise_stream(listener, stop_on_shutdown))
    }

    /// Find the UDP listen socket with `addr` from the sockets passed from systemd.
    /// The function will check if the found socket is bound to `addr`.
    ///
    /// See [`TokioEcdysis::systemd_listen_unix`] for more information.
    #[cfg(feature = "systemd_sockets")]
    pub async fn systemd_socket_udp(
        &self,
        name: String,
        addr: SocketAddr,
    ) -> Result<UdpSocket, SystemdSocketError> {
        let socket: std::net::UdpSocket = self
            .systemd_sock_of_proto(name, SockInfo::Udp(addr))
            .await?;
        socket.set_nonblocking(true)?;
        Ok(UdpSocket::from_std(socket)?)
    }

    /// Find the UDP stream socket with `addr` from the sockets passed from systemd.
    /// The function will check if the found socket is bound to `addr`.
    ///
    /// See [`TokioEcdysis::systemd_listen_unix`] for more information.
    #[cfg(feature = "systemd_sockets")]
    pub async fn systemd_stream_udp(
        &self,
        stop_on_shutdown: StopOnShutdown,
        name: String,
        addr: SocketAddr,
    ) -> Result<StoppableStream<UdpStream>, SystemdSocketError> {
        let socket = self.systemd_socket_udp(name, addr).await?;
        let (_s, reader) = UdpFramed::new(socket, BytesCodec::new()).split::<(Bytes, _)>();
        Ok(self
            .supervisor
            .read()
            .supervise_stream(reader, stop_on_shutdown))
    }
}

pub type TokioEcdysisUpgradeResult = Result<(ExitMode, ExitReason), String>;

struct TokioEcdysisUpgrader {
    tokio_ecdysis: Arc<TokioEcdysis>,
    triggers: Vec<(Trigger, ExitCondition)>,
    #[cfg(feature = "systemd_notify")]
    systemd_notifier: Option<SystemdNotifier>,
}

impl TokioEcdysisUpgrader {
    /// [`TokioEcdysisUpgrader::initialize()`] runs any initialization procedures (for instance,
    /// notifying systemd that the process is ready).
    async fn initialize(&mut self) -> Result<(), String> {
        #[cfg(feature = "systemd_notify")]
        if let Some(systemd_notifier) = &mut self.systemd_notifier {
            systemd_notifier
                .notify_ready()
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Initiate an upgrade. See [`Ecdysis::upgrade()`] for details.
    async fn upgrade(&mut self) -> Result<UpgradeFinished, String> {
        #[cfg(feature = "systemd_notify")]
        if let Some(systemd_notifier) = &mut self.systemd_notifier {
            systemd_notifier
                .notify_reloading()
                .await
                .map_err(|e| e.to_string())?;
        }

        let (tx, rx) = channel();
        let fds = self.tokio_ecdysis.inner.registry.get_fds_for_child();
        log::warn!("Ecdysis starting upgrade");
        thread::spawn(move || {
            if let Err(e) = tx.send(upgrade(fds)) {
                panic!(
                    "Cannot send upgrade result{}",
                    e.map_or_else(|e| format!(": {e}"), |()| "!".into())
                );
            }
        });
        rx.await.map_err(|e| e.to_string())
    }

    async fn monitor_triggers(&mut self) -> io::Result<(TriggerReason, ExitCondition)> {
        fn poll_triggers(
            triggers: &mut [(Trigger, ExitCondition)],
            cx: &mut Context,
        ) -> Poll<io::Result<(TriggerReason, ExitCondition)>> {
            for (trigger, ec) in triggers {
                match trigger.poll_next_unpin(cx) {
                    Poll::Ready(Some(result)) => return Poll::Ready(result.map(|o| (o, *ec))),
                    Poll::Ready(None) => unreachable!(), // triggers are infinite streams
                    Poll::Pending => (),
                }
            }

            Poll::Pending
        }

        let polling_fut = std::future::poll_fn(|cx| poll_triggers(&mut self.triggers, cx));
        polling_fut.await
    }

    async fn clean_up_triggers(&mut self) -> Result<(), String> {
        for (trigger, _ec) in self.triggers.drain(..) {
            trigger.cleanup().await?
        }
        Ok(())
    }

    async fn on_shutdown(mut self) -> Result<(), String> {
        self.clean_up_triggers().await?;

        #[cfg(feature = "systemd_notify")]
        {
            if let Some(systemd_notifier) = &mut self.systemd_notifier {
                systemd_notifier
                    .notify_stopping()
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn quit(&mut self, ec: ExitCondition) -> Result<(), String> {
        self.tokio_ecdysis.inner.quit();
        self.tokio_ecdysis
            .supervisor
            .write()
            .stop_all(ec)
            .map_err(|_| "Cannot stop supervised listeners!".into())
    }

    /// [`TokioEcdysisUpgrader::monitor()`] consumes the upgrader, starts monitoring the triggers,
    /// and executes the upgrade once it is triggered.
    async fn monitor(mut self) -> TokioEcdysisUpgradeResult {
        loop {
            // if the systemd_notify feature is enabled, this makes sure we (1) send READY=1 when the
            // process is ready; and (2) we also send READY=1 after a failed upgrade, so that
            // systemd knows we are ready to try again.
            self.initialize().await?;

            // poll the triggers
            let reason_condition = match self.monitor_triggers().await {
                Ok(reason_condition) => reason_condition,
                Err(e) => {
                    self.on_shutdown().await?;
                    return Err(format!("Encountered error while polling triggers: {e}"));
                }
            };

            let upgrade_reason = match reason_condition {
                (reason, ExitCondition::Upgrade) => reason,
                (reason, not_upgrade_condition) => {
                    log::warn!("Ecdysis stopping (reason: {reason:?})");

                    let exit_reason = match reason {
                        TriggerReason::Signal(kind) => ExitReason::Signal(kind),
                        TriggerReason::UnixStream(path, stream) => {
                            // We mem::forget the open stream here so that the stream is not
                            // dropped until the whole process shuts down.
                            mem::forget(stream);
                            ExitReason::UnixListener(path)
                        }
                    };

                    self.quit(not_upgrade_condition)?;

                    if let ExitCondition::PartialStop = not_upgrade_condition {
                        // Special case: on a partial stop, we only clean up triggers and skip the
                        // rest of the shutdown procedure. In particular, we don't send any further
                        // notifications to systemd --- this becomes responsibility of the caller.
                        self.clean_up_triggers().await?;
                        #[cfg(feature = "systemd_notify")]
                        if let Some(systemd_notifier) = self.systemd_notifier {
                            return Ok((
                                ExitMode::PartialStopWithSystemd(systemd_notifier),
                                exit_reason,
                            ));
                        }
                        return Ok((ExitMode::PartialStop, exit_reason));
                    }

                    self.on_shutdown().await?;
                    return Ok((ExitMode::FullStop, exit_reason));
                }
            };

            // at this point, we know we need to upgrade
            log::warn!("Ecdysis starting upgrade (reason: {upgrade_reason:?})");
            return match self.upgrade().await {
                Ok(upgrade_finished) => match upgrade_finished {
                    Ok(()) => {
                        // successful upgrade
                        log::info!("Upgrade successful");
                        self.quit(ExitCondition::Upgrade)?;
                        // no "on_shutdown" and no cleanup of triggers after an upgrade
                        // also, no need to send READY again to systemd, that's handled by
                        // our child when *they* are ready.
                        Ok((
                            ExitMode::Upgrade,
                            match upgrade_reason {
                                TriggerReason::Signal(kind) => ExitReason::Signal(kind),
                                TriggerReason::UnixStream(path, _stream) => {
                                    ExitReason::UnixListener(path)
                                }
                            },
                        ))
                    }
                    Err(e) => {
                        // something went wrong during the upgrade, try again
                        log::warn!("Upgrade failed: {e}");
                        log::warn!("Ecdysis returning to listening state!");

                        // FIXME: write_pidfile is potentially blocking
                        let _ = self.tokio_ecdysis.inner.write_pidfile();
                        continue;
                    }
                },
                Err(err_str) => {
                    // either a channel error or an error notifying systemd (if enabled)
                    // Die and let any external process supervisors take over
                    self.on_shutdown().await?;
                    Err(format!("Encountered a problem during upgrade: {err_str}"))
                }
            };
        }
    }
}
