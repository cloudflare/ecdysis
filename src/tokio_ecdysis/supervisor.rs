use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt};
use tokio::sync::watch::{channel, Receiver, Sender};
use tokio_stream::wrappers::WatchStream;

use super::ExitCondition;

/// None means run
type RunState = Option<ExitCondition>;

#[allow(dead_code)]
pub(crate) struct Supervisor {
    tx: Sender<RunState>,
    rx: Receiver<RunState>,
    state: RunState,
}

impl Supervisor {
    pub(crate) fn new() -> Self {
        let state = None;
        let (tx, rx) = channel(state);

        Self { tx, rx, state }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

pub enum StopOnShutdown {
    Yes,
    No,
}

impl StopOnShutdown {
    fn should_continue(&self, state: &RunState) -> bool {
        match state {
            None => true,
            Some(ExitCondition::Upgrade) => false,
            // The following arm makes Self::No return true, and Self::yes return false.
            Some(ExitCondition::Stop) | Some(ExitCondition::PartialStop) => {
                matches!(self, StopOnShutdown::No)
            }
        }
    }
}

impl Supervisor {
    pub(crate) fn stop_all(&mut self, ec: ExitCondition) -> Result<(), String> {
        log::warn!("Parent stopping listening streams for {ec:?}");
        self.tx
            .send(Some(ec))
            .map_err(|e| format!("Error stopping supervisor: {e:?}"))
    }

    pub(crate) fn supervise_stream<S>(
        &self,
        stream: S,
        stop_on_shutdown: StopOnShutdown,
    ) -> StoppableStream<S>
    where
        S: Stream + Unpin,
    {
        StoppableStream::new(stream, stop_on_shutdown, self.rx.clone())
    }

    pub(crate) fn supervise<S>(&self, socket: S, stop_on_shutdown: StopOnShutdown) -> Stoppable<S> {
        Stoppable::new(socket, stop_on_shutdown, self.rx.clone())
    }
}

pub struct StoppableStream<S> {
    stream: S,
    stop_on_shutdown: StopOnShutdown,
    rx: WatchStream<RunState>,
    state: RunState,
}

impl<S> StoppableStream<S> {
    fn new(stream: S, stop_on_shutdown: StopOnShutdown, rx: Receiver<RunState>) -> Self {
        Self {
            stream,
            stop_on_shutdown,
            rx: WatchStream::new(rx),
            state: None,
        }
    }
}

impl<S> Stream for StoppableStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(state)) = self.rx.poll_next_unpin(cx) {
            self.state = state;
        }
        if self.stop_on_shutdown.should_continue(&self.state) {
            self.stream.poll_next_unpin(cx)
        } else {
            Poll::Ready(None)
        }
    }
}

/// A wrapper around something that has long-running async functions which should be interrupted
/// during a shutdown.
pub struct Stoppable<S> {
    s: S,
    rx: Receiver<RunState>,
    stop_on_shutdown: StopOnShutdown,
}

impl<S> Stoppable<S> {
    fn new(s: S, stop_on_shutdown: StopOnShutdown, rx: Receiver<RunState>) -> Self {
        Self {
            s,
            rx,
            stop_on_shutdown,
        }
    }

    /// Returns a direct reference to inner object, allowing direct _uninterrupted_ operations.
    pub fn as_unstoppable_ref(&self) -> &S {
        &self.s
    }

    /// Like [`Stoppable::as_unstoppable_ref`], but with mutable access.
    pub fn as_unstoppable_mut(&mut self) -> &mut S {
        &mut self.s
    }

    /// Takes a closure given a reference to the inner object. The closure should return a future
    /// that does the desired operation on the inner object. The future will be run, but
    /// interrupted as soon as stopped by the associated Supervisor. None is returned if the future
    /// was interrupted.
    pub async fn until_stopped<'a, O, F>(&'a self, f: impl FnOnce(&'a S) -> F) -> Option<O>
    where
        F: Future<Output = O>,
    {
        let mut rx = self.rx.clone();

        let stopped_fut = async {
            while self.stop_on_shutdown.should_continue(&rx.borrow()) {
                rx.changed().await.unwrap();
            }
        };

        tokio::select! {
           output = f(&self.s) => Some(output),
           _ = stopped_fut => None,
        }
    }

    /// Like [`Stoppable::until_stopped()`], but with mutable access.
    pub async fn until_stopped_mut<'a, O, F>(
        &'a mut self,
        f: impl FnOnce(&'a mut S) -> F,
    ) -> Option<O>
    where
        F: Future<Output = O>,
    {
        let rx = &mut self.rx;
        let stop_on_shutdown = &self.stop_on_shutdown;

        let stopped_fut = async {
            while stop_on_shutdown.should_continue(&rx.borrow()) {
                rx.changed().await.unwrap();
            }
        };

        tokio::select! {
           output = f(&mut self.s) => Some(output),
           _ = stopped_fut => None,
        }
    }
}
