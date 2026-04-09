use std::{
    io,
    net::{TcpListener, UdpSocket},
    os::unix::{io::AsRawFd, net::UnixListener},
    path::PathBuf,
};

#[cfg(feature = "systemd_sockets")]
use std::os::fd::{FromRawFd, RawFd};

use crate::registry::{ListenerInfo, SockInfo};

pub trait Listener: AsRawFd {
    fn info(&self) -> io::Result<ListenerInfo>;
}

// TODO: impl standard listeners
impl Listener for TcpListener {
    fn info(&self) -> io::Result<ListenerInfo> {
        Ok(ListenerInfo {
            fd: self.as_raw_fd(),
            sock_info: SockInfo::Tcp(self.local_addr()?),
        })
    }
}

impl Listener for UdpSocket {
    fn info(&self) -> io::Result<ListenerInfo> {
        Ok(ListenerInfo {
            fd: self.as_raw_fd(),
            sock_info: SockInfo::Udp(self.local_addr()?),
        })
    }
}

impl Listener for UnixListener {
    fn info(&self) -> io::Result<ListenerInfo> {
        let sockaddr = self.local_addr()?;
        let addr = sockaddr.as_pathname().map(PathBuf::from);
        Ok(ListenerInfo {
            fd: self.as_raw_fd(),
            sock_info: SockInfo::Unix(addr),
        })
    }
}

// UnixSeqpacketListener does not implment FromRawFd because its from_raw_fd()
// returns a Result instead of Self. We use this trait to unify the interface
// of our listeners.
#[cfg(feature = "systemd_sockets")]
pub(crate) trait TryFromRawFd {
    unsafe fn try_from_raw_fd(fd: RawFd) -> io::Result<Self>
    where
        Self: Sized;
}

#[cfg(feature = "systemd_sockets")]
impl TryFromRawFd for UnixListener {
    unsafe fn try_from_raw_fd(fd: RawFd) -> io::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::from_raw_fd(fd))
    }
}

#[cfg(feature = "systemd_sockets")]
impl TryFromRawFd for TcpListener {
    unsafe fn try_from_raw_fd(fd: RawFd) -> io::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::from_raw_fd(fd))
    }
}

#[cfg(feature = "systemd_sockets")]
impl TryFromRawFd for UdpSocket {
    unsafe fn try_from_raw_fd(fd: RawFd) -> io::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::from_raw_fd(fd))
    }
}
