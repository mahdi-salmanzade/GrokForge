//! Bridges the agent's [`Approver`] calls to the interactive UI: the agent awaits a decision
//! while the UI shows a modal and resolves it on a keypress.

use async_trait::async_trait;
use grokforge_core::Approver;
use grokforge_protocol::{ApprovalRequest, Decision};
use tokio::sync::{mpsc, oneshot};

/// An approval the UI must resolve, with the channel to answer on.
#[derive(Debug)]
pub struct PendingApproval {
    pub request: ApprovalRequest,
    pub respond: oneshot::Sender<Decision>,
}

/// An [`Approver`] that forwards each request to the UI over a channel and awaits the answer.
#[derive(Debug, Clone)]
pub struct ChannelApprover {
    tx: mpsc::UnboundedSender<PendingApproval>,
}

impl ChannelApprover {
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PendingApproval>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn request(&self, req: ApprovalRequest) -> Decision {
        let (respond, wait) = oneshot::channel();
        if self
            .tx
            .send(PendingApproval {
                request: req,
                respond,
            })
            .is_err()
        {
            // UI is gone; fail safe.
            return Decision::Deny;
        }
        wait.await.unwrap_or(Decision::Deny)
    }
}
