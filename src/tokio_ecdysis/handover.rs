//! Tokio adapter for the blocking transactional handover API.

use tokio::task::{self, JoinError};

use crate::handover::HandoverPeer;

/// A handover peer whose transaction can run without blocking a Tokio worker.
///
/// [`run`](Self::run) owns the peer for the duration of the blocking operation. This makes
/// cancellation safe: cancelling the awaiting task does not expose the peer for concurrent use,
/// and the configured handover timeout eventually ends the blocking operation.
pub struct TokioHandoverPeer {
    inner: HandoverPeer,
}

impl TokioHandoverPeer {
    pub(crate) fn new(inner: HandoverPeer) -> Self {
        Self { inner }
    }

    /// Run one complete synchronous transaction on Tokio's blocking pool.
    ///
    /// The peer is returned so a parent can serve a later upgrade attempt after an abort or
    /// timeout. `operation` commonly returns a `Result`, which remains nested inside the join
    /// result so thread failure and handover failure stay distinct.
    pub async fn run<F, T>(mut self, operation: F) -> Result<(Self, T), JoinError>
    where
        F: FnOnce(&mut HandoverPeer) -> T + Send + 'static,
        T: Send + 'static,
    {
        task::spawn_blocking(move || {
            let output = operation(&mut self.inner);
            (self, output)
        })
        .await
    }

    pub fn into_inner(self) -> HandoverPeer {
        self.inner
    }
}

impl From<HandoverPeer> for TokioHandoverPeer {
    fn from(value: HandoverPeer) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use crate::handover::{HandoverPeer, SupportedVersions};

    use super::TokioHandoverPeer;

    #[tokio::test]
    async fn runs_a_transaction_off_the_runtime_workers() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let parent = TokioHandoverPeer::new(HandoverPeer::new(parent_socket).unwrap());
        let child = TokioHandoverPeer::new(HandoverPeer::new(child_socket).unwrap());

        let parent_operation = parent.run(|peer| {
            let request = peer.receive_request()?;
            let outgoing = request.begin(1)?;
            outgoing.finish()?.wait()?.commit()
        });
        let child_operation = child.run(|peer| {
            let mut incoming = peer.request(SupportedVersions::exact(1))?;
            assert!(incoming.receive_item()?.is_none());
            incoming.prepare()?.wait_for_commit()
        });

        let (parent_result, child_result) = tokio::join!(parent_operation, child_operation);
        let (_parent, handover_result) = parent_result.unwrap();
        handover_result.unwrap();
        let (_child, handover_result) = child_result.unwrap();
        let _commit = handover_result.unwrap();
    }
}
