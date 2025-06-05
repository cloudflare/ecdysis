use std::{io, net::SocketAddr, path::PathBuf, sync::Mutex};

use crate::utils::{clone_fd, close_fd_quiet, set_cloexec};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub(crate) enum SockInfo {
    // Due to the fact that bincode is not a self-describing data format,
    // new fields must be added at the end of the enum in order to
    // preserve forward-compatibility.
    Unix(Option<PathBuf>),
    Tcp(SocketAddr),
    Udp(SocketAddr),

    /// An unbound unix datagram is not bound to any file path. The stored string only identifies
    /// it within Ecdysis's registry.
    UnboundUnixDatagram(String),

    UnixSeqpacket(Option<PathBuf>),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ListenerInfo {
    pub(crate) fd: i32,
    pub(crate) sock_info: SockInfo,
}

/// File descriptor tracking for Ecdysis. This maintains two vecs of ListenerInfos.
///
/// The first, `used_fds`, is for tracking the files that have been opened via `Ecdysis::listen_*`
/// methods.  These ListenerInfos are serialized and written to the child via pipe, where they wind
/// up in `inherited_fds`. The `add` method will only put `ListenerInfo` instances into `used_fds`.
/// Note: the file descriptor stored in `used_fds` is actually a duplicate (via the `dup` syscall)
/// so the semantics of the fds in the Listener object is not changed.
///
/// The second, `inherited_fds` is initialized during an upgrade by reading from a pipe provided by
/// the parent. The `ListenerRegistry::inherit()` method searches in inherited_fds for a match. If
/// found the match is removed from `inherited_fds` and returned to the caller.
pub(crate) struct ListenerRegistry {
    inherited_fds: Mutex<Vec<ListenerInfo>>,
    used_fds: Mutex<Vec<ListenerInfo>>,
}

impl ListenerRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inherited_fds: Mutex::new(Vec::new()),
            used_fds: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn from_inherited(inherited: Vec<ListenerInfo>) -> Self {
        Self {
            inherited_fds: Mutex::new(inherited),
            used_fds: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn inherit(&self, sock_info: SockInfo) -> Option<i32> {
        let fd = self.take(&sock_info)?;
        let listener_info = ListenerInfo { fd, sock_info };
        self.add(listener_info).ok()?;
        Some(fd)
    }

    pub(crate) fn take(&self, sock_info: &SockInfo) -> Option<i32> {
        let mut fds = self
            .inherited_fds
            .lock()
            .expect("Cannot take lock on FdRegistry");
        match fds.iter().position(|li| &li.sock_info == sock_info) {
            Some(p) => {
                let li = fds.remove(p);
                let fd = li.fd;
                // at this point, the inherited fd is not CLOEXEC, fix that
                set_cloexec(fd);
                Some(fd)
            }
            None => None,
        }
    }

    pub(crate) fn add(&self, mut item: ListenerInfo) -> Result<(), io::Error> {
        let mut fds = self
            .used_fds
            .lock()
            .expect("Cannot take lock on FdRegisry!");

        // First make a copy of the file descriptor. (This copy preserves fd attrs)
        let new_fd = clone_fd(item.fd)?;

        // Next replace the fd in the ListenerInfo item with the duped copy. This is what the child
        // process will use when rebuilding the state.
        item.fd = new_fd;

        fds.push(item);
        Ok(())
    }

    pub(crate) fn close_used(&self) {
        let mut fds = self
            .used_fds
            .lock()
            .expect("Cannot take lock on FdRegisry!");
        for li in fds.iter() {
            close_fd_quiet(li.fd);
        }
        fds.clear();
    }

    pub(crate) fn close_inherited(&mut self) {
        let mut fds = self
            .inherited_fds
            .lock()
            .expect("Cannot take lock on FdRegisry!");
        for li in fds.iter() {
            close_fd_quiet(li.fd);
        }
        fds.clear();
    }

    pub(crate) fn get_fds_for_child(&self) -> Vec<ListenerInfo> {
        let fds = self
            .used_fds
            .lock()
            .expect("Cannot take lock on FdRegistry");
        (*fds).clone()
    }
}

impl Drop for ListenerRegistry {
    fn drop(&mut self) {
        self.close_used();
        self.close_inherited();
    }
}
