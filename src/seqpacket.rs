use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(feature = "systemd_sockets")]
use std::os::fd::RawFd;

use futures::Stream;

use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

use crate::{
    listener::Listener,
    registry::{ListenerInfo, SockInfo},
};

#[cfg(feature = "systemd_sockets")]
use crate::listener::TryFromRawFd;

impl Listener for UnixSeqpacketListener {
    fn info(&self) -> io::Result<ListenerInfo> {
        let sockaddr = self.local_addr()?;
        Ok(ListenerInfo {
            fd: self.as_raw_fd(),
            sock_info: SockInfo::UnixSeqpacket(Some(sockaddr)),
        })
    }
}

#[cfg(feature = "systemd_sockets")]
impl TryFromRawFd for UnixSeqpacketListener {
    unsafe fn try_from_raw_fd(fd: RawFd) -> io::Result<Self>
    where
        Self: Sized,
    {
        Self::from_raw_fd(fd)
    }
}

pub struct UnixSeqpacketListenerStream(UnixSeqpacketListener);

impl UnixSeqpacketListenerStream {
    pub fn new(listener: UnixSeqpacketListener) -> Self {
        Self(listener)
    }
}

impl Stream for UnixSeqpacketListenerStream {
    type Item = io::Result<UnixSeqpacket>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_accept(cx).map(Some)
    }
}

/// Set or unset nonblocking flag on socket file descriptor.
///
/// UnixSeqpacketListener does not provide a set_nonblocking method, so we
/// implement one here.
#[cfg(feature = "systemd_sockets")]
pub fn set_nonblocking(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};

    let mut oflags = OFlag::from_bits(fcntl(fd, FcntlArg::F_GETFL)?)
        .ok_or(io::Error::other("fcntl F_GETFL returned invalid flags"))?;
    oflags.set(OFlag::O_NONBLOCK, nonblocking);
    fcntl(fd, FcntlArg::F_SETFL(oflags))?;
    Ok(())
}
