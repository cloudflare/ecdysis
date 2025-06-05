use std::{
    io,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{ready, Stream, StreamExt};
use tokio::{net::UnixStream, signal::unix::SignalKind};
use tokio_stream::wrappers::{SignalStream, UnixListenerStream};

use crate::tokio_ecdysis::StoppableStream;

/// A Trigger is anything that can trigger a reload or shutdown in TokioEcdysis
pub(crate) enum Trigger {
    Signal(SignalKind, SignalStream),
    Uds(PathBuf, StoppableStream<UnixListenerStream>),
}

/// Holds something to identify the trigger that was triggered as well as state that should be
/// maintained between upgrader state transitions.
#[derive(Debug)]
pub(crate) enum TriggerReason {
    Signal(SignalKind),
    UnixStream(PathBuf, UnixStream),
}

impl Trigger {
    pub(crate) async fn cleanup(self) -> Result<(), String> {
        // explicit match since we might add more Triggers in the future
        match self {
            Trigger::Signal(_kind, _stream) => Ok(()),
            Trigger::Uds(path, _listener_stream) => {
                // cleanup path
                // this branch also takes ownership of the listener stream, which is then dropped
                // at the end of the scope. neat!

                log::debug!("Removing {path:?}");
                tokio::fs::remove_file(path.clone())
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }
}

impl Stream for Trigger {
    type Item = io::Result<TriggerReason>;

    /// TODO: Document that this never returns None
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            Self::Signal(kind, signal_stream) => match ready!(signal_stream.poll_next_unpin(cx)) {
                None => unreachable!(), // SignalStream is documented to be infinite.
                Some(()) => Poll::Ready(Some(Ok(TriggerReason::Signal(*kind)))),
            },
            Self::Uds(path, listener_stream) => match ready!(listener_stream.poll_next_unpin(cx)) {
                None => Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Socket shut down unexpectedly.",
                )))),
                Some(Err(e)) => Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("Unexpected error accepting connection on socket: {e}"),
                )))),
                Some(Ok(stream)) => {
                    Poll::Ready(Some(Ok(TriggerReason::UnixStream(path.clone(), stream))))
                }
            },
        }
    }
}
