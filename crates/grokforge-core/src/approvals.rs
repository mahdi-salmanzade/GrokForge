//! The approval engine: the pure 4-policy × 3-mode decision table, plus the [`Approver`]
//! abstraction a frontend implements to answer prompts. Keeping the decision logic pure makes
//! all 12 cells unit-testable without a terminal.

use std::path::PathBuf;

use async_trait::async_trait;
use grokforge_protocol::{ApprovalKind, ApprovalPolicy, ApprovalRequest, Decision, SandboxMode};

/// Whether a tool needs approval before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalNeed {
    /// Pure reads — never gated.
    None,
    /// Gated by policy × sandbox mode; carries the boundary the tool wants to cross.
    Gated(ApprovalKind),
    /// Always gated unless the policy is `Never` (e.g. an un-allowlisted MCP tool).
    Always(ApprovalKind),
}

/// The outcome of consulting the decision table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Proceed without asking.
    Allow,
    /// Ask the user, presenting this boundary.
    Ask(ApprovalKind),
}

/// The core decision table. Given the active policy and sandbox mode and what the tool needs,
/// decide whether to run, ask, or (via `OnFailure`) run-then-escalate.
///
/// `Never` and `OnFailure` both gate to `Allow`, but for different reasons (never-ask vs
/// run-then-escalate-on-block), so they are kept as separate, documented arms.
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn gate(policy: ApprovalPolicy, mode: SandboxMode, need: &ApprovalNeed) -> Gate {
    match need {
        ApprovalNeed::None => Gate::Allow,
        ApprovalNeed::Always(kind) => match policy {
            ApprovalPolicy::Never => Gate::Allow,
            _ => Gate::Ask(kind.clone()),
        },
        ApprovalNeed::Gated(kind) => match policy {
            // Never ask; the tool runs (and, in a sandbox, may still be blocked).
            ApprovalPolicy::Never => Gate::Allow,
            // Run sandboxed; the denial classifier escalates only if the sandbox blocks it.
            ApprovalPolicy::OnFailure => Gate::Allow,
            // Ask before anything that could mutate or exec.
            ApprovalPolicy::Untrusted => Gate::Ask(kind.clone()),
            // Auto-run what fits the sandbox; ask to exceed it.
            ApprovalPolicy::OnRequest => {
                if fits_sandbox(kind, mode) {
                    Gate::Allow
                } else {
                    Gate::Ask(kind.clone())
                }
            }
        },
    }
}

/// Whether an operation can be carried out entirely within the given sandbox mode (so it does
/// not need to escape the box and therefore does not need approval under `OnRequest`).
fn fits_sandbox(kind: &ApprovalKind, mode: SandboxMode) -> bool {
    match kind {
        // Writes and edits fit any mode that permits writing.
        ApprovalKind::WriteFile { .. } | ApprovalKind::ApplyPatch { .. } => {
            mode != SandboxMode::ReadOnly
        }
        // Sandboxed exec fits workspace-write and full-access; read-only can't run mutating cmds.
        ApprovalKind::ExecCommand { .. } => mode != SandboxMode::ReadOnly,
        // Network only fits when the mode allows network at all (workspace-write denies it).
        ApprovalKind::Network { .. } => mode == SandboxMode::DangerFullAccess,
        // Git mutations run in the host process and MCP calls reach outside — always surfaced
        // (except under the `Never` policy handled above).
        ApprovalKind::GitMutation { .. } | ApprovalKind::McpToolCall { .. } => false,
    }
}

/// A frontend's approval handler.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Answer an approval request.
    async fn request(&self, req: ApprovalRequest) -> Decision;
}

/// A pre-granted boundary for non-interactive runs (`--allow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowRule {
    /// Everything (`--yolo`).
    All,
    /// Any network access.
    Network,
    /// Writes at or under this path.
    Write(PathBuf),
    /// Commands whose program/first token starts with this prefix.
    CmdPrefix(String),
}

/// The headless approver: auto-denies with feedback unless a boundary was pre-granted. This is
/// "deny and continue" — the model is told why and can route around the denial.
#[derive(Debug, Clone, Default)]
pub struct AutoApprover {
    pub rules: Vec<AllowRule>,
}

impl AutoApprover {
    #[must_use]
    pub fn new(rules: Vec<AllowRule>) -> Self {
        Self { rules }
    }

    /// A fully permissive approver (`--yolo`).
    #[must_use]
    pub fn yolo() -> Self {
        Self {
            rules: vec![AllowRule::All],
        }
    }

    #[allow(clippy::match_same_arms)] // distinct patterns that happen to share a body
    fn permits(&self, kind: &ApprovalKind) -> bool {
        self.rules.iter().any(|rule| match (rule, kind) {
            (AllowRule::All, _) => true,
            (AllowRule::Network, ApprovalKind::Network { .. }) => true,
            (AllowRule::Write(root), ApprovalKind::WriteFile { path }) => path.starts_with(root),
            (AllowRule::Write(root), ApprovalKind::ApplyPatch { files }) => {
                files.iter().all(|f| f.starts_with(root))
            }
            (AllowRule::CmdPrefix(prefix), ApprovalKind::ExecCommand { command, .. }) => {
                command.first().is_some_and(|p| p.starts_with(prefix))
            }
            _ => false,
        })
    }
}

#[async_trait]
impl Approver for AutoApprover {
    // The trait takes the request by value (an approver may store it); this impl only borrows.
    #[allow(clippy::needless_pass_by_value)]
    async fn request(&self, req: ApprovalRequest) -> Decision {
        if self.permits(&req.kind) {
            Decision::ApproveForSession
        } else {
            Decision::DenyWithFeedback(
                "auto-denied in non-interactive mode; re-run with --allow <boundary> or --yolo to permit this".to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grokforge_protocol::ApprovalPolicy::{Never, OnFailure, OnRequest, Untrusted};
    use grokforge_protocol::SandboxMode::{DangerFullAccess, ReadOnly, WorkspaceWrite};

    fn write_need() -> ApprovalNeed {
        ApprovalNeed::Gated(ApprovalKind::WriteFile {
            path: PathBuf::from("/proj/a.rs"),
        })
    }

    fn is_allow(g: &Gate) -> bool {
        matches!(g, Gate::Allow)
    }

    #[test]
    fn all_twelve_cells_for_an_in_workspace_write() {
        // The canonical matrix (docs/design/04-ux-spec.md §4.1).
        let need = write_need();
        // policy, mode -> expect Allow?
        let cases = [
            (Never, ReadOnly, true),
            (Never, WorkspaceWrite, true),
            (Never, DangerFullAccess, true),
            (Untrusted, ReadOnly, false),
            (Untrusted, WorkspaceWrite, false),
            (Untrusted, DangerFullAccess, false),
            (OnRequest, ReadOnly, false),
            (OnRequest, WorkspaceWrite, true),
            (OnRequest, DangerFullAccess, true),
            (OnFailure, ReadOnly, true),
            (OnFailure, WorkspaceWrite, true),
            (OnFailure, DangerFullAccess, true),
        ];
        for (policy, mode, expect_allow) in cases {
            assert_eq!(
                is_allow(&gate(policy, mode, &need)),
                expect_allow,
                "policy={policy:?} mode={mode:?}"
            );
        }
    }

    #[test]
    fn pure_reads_are_never_gated() {
        assert!(is_allow(&gate(Untrusted, ReadOnly, &ApprovalNeed::None)));
    }

    #[test]
    fn network_only_fits_full_access() {
        let need = ApprovalNeed::Gated(ApprovalKind::Network {
            host: "example.com".to_string(),
        });
        assert!(!is_allow(&gate(OnRequest, WorkspaceWrite, &need)));
        assert!(is_allow(&gate(OnRequest, DangerFullAccess, &need)));
    }

    #[tokio::test]
    async fn auto_approver_denies_by_default_but_honors_allow() {
        use grokforge_protocol::ApprovalId;
        let kind = ApprovalKind::WriteFile {
            path: PathBuf::from("/proj/src/a.rs"),
        };
        let req = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: kind.clone(),
            reason: "write".to_string(),
        };

        let deny = AutoApprover::default().request(req.clone()).await;
        assert!(matches!(deny, Decision::DenyWithFeedback(_)));

        let allow = AutoApprover::new(vec![AllowRule::Write(PathBuf::from("/proj"))])
            .request(req)
            .await;
        assert!(allow.is_approved());
    }
}
