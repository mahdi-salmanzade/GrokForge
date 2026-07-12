//! Bridges the agent's [`Approver`] calls to the interactive UI: the agent awaits a decision
//! while the UI shows a modal and resolves it on a keypress.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use grokforge_core::Approver;
use grokforge_protocol::{ApprovalKind, ApprovalRequest, Decision};
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
    session_grants: Arc<Mutex<Vec<ApprovalKind>>>,
}

impl ChannelApprover {
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PendingApproval>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                session_grants: Arc::new(Mutex::new(Vec::new())),
            },
            rx,
        )
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn request(&self, req: ApprovalRequest) -> Decision {
        let kind = req.kind.clone();
        if self
            .session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&kind)
        {
            return Decision::ApproveForSession;
        }
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
        let decision = wait.await.unwrap_or(Decision::Deny);
        if decision == Decision::ApproveForSession {
            let mut grants = self
                .session_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !grants.contains(&kind) {
                grants.push(kind);
            }
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::path::PathBuf;
    use std::time::Duration;

    use grokforge_core::Approver as _;
    use grokforge_protocol::{ApprovalId, SandboxMode};

    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::ExecCommand {
                command: vec!["cargo".to_string(), "test".to_string()],
                cwd: PathBuf::from("/workspace"),
                sandbox: SandboxMode::WorkspaceWrite,
                escalation_of: None,
            },
            reason: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn approve_for_session_is_remembered_for_the_same_boundary() {
        let (approver, mut pending) = ChannelApprover::new();
        let first = tokio::spawn({
            let approver = approver.clone();
            async move { approver.request(request()).await }
        });
        pending
            .recv()
            .await
            .expect("pending approval")
            .respond
            .send(Decision::ApproveForSession)
            .expect("response accepted");
        assert_eq!(
            first.await.expect("request task"),
            Decision::ApproveForSession
        );

        let second = tokio::time::timeout(Duration::from_millis(100), approver.request(request()))
            .await
            .expect("cached approval should not wait for UI");
        assert_eq!(second, Decision::ApproveForSession);
        assert!(pending.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_write_grant_is_bound_to_the_physical_path_identity() {
        let (approver, mut pending) = ChannelApprover::new();
        let write_request = |path: &str| ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::WriteFile {
                path: PathBuf::from(path),
            },
            reason: "write".into(),
        };
        let first = tokio::spawn({
            let approver = approver.clone();
            let request = write_request("/outside/physical-a/file");
            async move { approver.request(request).await }
        });
        pending
            .recv()
            .await
            .expect("first pending approval")
            .respond
            .send(Decision::ApproveForSession)
            .expect("first response accepted");
        assert_eq!(
            first.await.expect("first request task"),
            Decision::ApproveForSession
        );

        let second = tokio::spawn({
            let approver = approver.clone();
            let request = write_request("/outside/physical-b/file");
            async move { approver.request(request).await }
        });
        let second_pending = tokio::time::timeout(Duration::from_secs(1), pending.recv())
            .await
            .expect("retargeted path must ask again")
            .expect("second pending approval");
        second_pending
            .respond
            .send(Decision::Deny)
            .expect("second response accepted");
        assert_eq!(second.await.expect("second request task"), Decision::Deny);
    }
}
