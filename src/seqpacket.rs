use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

use crate::{
    listener::Listener,
    registry::{ListenerInfo, SockInfo},
};

impl Listener for UnixSeqpacketListener {
    fn info(&self) -> io::Result<ListenerInfo> {
        let sockaddr = self.local_addr()?;
        Ok(ListenerInfo {
            fd: self.as_raw_fd(),
            sock_info: SockInfo::UnixSeqpacket(Some(sockaddr)),
        })
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
