use std::{
    io,
    net::{TcpListener, UdpSocket},
    os::unix::{io::AsRawFd, net::UnixListener},
    path::PathBuf,
};

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
