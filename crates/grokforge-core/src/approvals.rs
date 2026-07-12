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
    /// The requested action exceeds the active sandbox. `OnFailure` tries it confined first and
    /// asks only after a classified denial; `OnRequest` asks before the first attempt.
    OutsideSandbox(ApprovalKind),
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
        ApprovalNeed::OutsideSandbox(kind) => match policy {
            ApprovalPolicy::Never | ApprovalPolicy::OnFailure => Gate::Allow,
            ApprovalPolicy::Untrusted | ApprovalPolicy::OnRequest => Gate::Ask(kind.clone()),
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
        // Executing a command fits every enforcing sandbox mode. A read-only policy may deny the
        // command's attempted writes later, but approving the command itself must not pre-grant
        // those writes.
        ApprovalKind::ExecCommand { .. } => true,
        // Network only fits when the mode allows network at all (workspace-write denies it).
        ApprovalKind::Network { .. } => mode == SandboxMode::DangerFullAccess,
        // Git mutations run in the host process and MCP calls reach outside — always surfaced
        // (except under the `Never` policy handled above).
        ApprovalKind::GitMutation { .. }
        | ApprovalKind::McpToolCall { .. }
        | ApprovalKind::SandboxEscalation { .. } => false,
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
    rules: Vec<AllowRule>,
}

impl AutoApprover {
    #[must_use]
    pub fn new(rules: Vec<AllowRule>) -> Self {
        let rules = rules
            .into_iter()
            .filter_map(|rule| match rule {
                // Bind a write grant to its physical root once. Re-resolving this path for every
                // request would let a sandboxed command replace it with a symlink and move the
                // grant outside its original boundary.
                AllowRule::Write(root) => crate::path_safety::canonicalize_allow_missing(&root)
                    .ok()
                    .map(AllowRule::Write),
                other => Some(other),
            })
            .collect();
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
            (AllowRule::Write(root), ApprovalKind::WriteFile { path }) => within(root, path),
            (AllowRule::Write(root), ApprovalKind::ApplyPatch { files }) => {
                files.iter().all(|f| within(root, f))
            }
            (
                AllowRule::CmdPrefix(prefix),
                ApprovalKind::ExecCommand {
                    command,
                    escalation_of: None,
                    ..
                },
            ) => command_prefix_matches(prefix, command),
            (
                AllowRule::Network,
                ApprovalKind::ExecCommand {
                    escalation_of: Some(grokforge_protocol::DenialClass::Network),
                    ..
                },
            ) => true,
            // Write and command-prefix grants never imply permission to escape a sandbox. An
            // explicit `All` grant above is required for filesystem escalation. Network remains
            // represented on ExecCommand for its narrowly-scoped grant.
            (_, ApprovalKind::SandboxEscalation { .. }) => false,
            _ => false,
        })
    }
}

fn within(root: &std::path::Path, path: &std::path::Path) -> bool {
    // `root` was physically bound when the AutoApprover was created. Resolve only the requested
    // target, so later symlink replacement cannot move the stored grant.
    let Ok(path) = crate::path_safety::canonicalize_allow_missing(path) else {
        return false;
    };
    path.starts_with(root)
}

fn command_prefix_matches(prefix: &str, command: &[String]) -> bool {
    fn simple_words(raw: &str) -> Option<Vec<String>> {
        // A CmdPrefix grant applies to `/bin/sh -c` input, not to arbitrary shell programs.  Only
        // accept a deliberately small grammar with literal ASCII words separated by spaces.  In
        // particular, quotes, escapes, expansion, substitutions, redirections, controls, and
        // command separators all fail closed.
        if raw.is_empty()
            || raw.bytes().any(|byte| {
                !(byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b' ' | b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'='
                    ))
            })
        {
            return None;
        }
        let words: Vec<_> = raw
            .split(' ')
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect();
        (!words.is_empty()).then_some(words)
    }

    let Some(prefix) = simple_words(prefix) else {
        return false;
    };
    let actual = if command.len() == 1 {
        let Some(actual) = simple_words(&command[0]) else {
            return false;
        };
        actual
    } else {
        let Some(actual) = simple_words(&command.join(" ")) else {
            return false;
        };
        actual
    };
    actual.len() >= prefix.len() && actual.iter().zip(prefix.iter()).all(|(a, b)| a == b)
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

    #[test]
    fn command_execution_fits_readonly_without_pregranting_writes() {
        let need = ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command: vec!["touch file".into()],
            cwd: PathBuf::from("/proj"),
            sandbox: ReadOnly,
            escalation_of: None,
        });
        assert!(is_allow(&gate(OnRequest, ReadOnly, &need)));
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

    #[tokio::test]
    async fn command_allow_is_an_exact_shell_safe_prefix() {
        use grokforge_protocol::ApprovalId;

        let approver = AutoApprover::new(vec![AllowRule::CmdPrefix("git".into())]);
        let decision_for = |parts: &[&str]| ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::ExecCommand {
                command: parts.iter().map(|part| (*part).to_string()).collect(),
                cwd: PathBuf::from("/proj"),
                sandbox: SandboxMode::WorkspaceWrite,
                escalation_of: None,
            },
            reason: "test".into(),
        };
        assert!(
            approver
                .request(decision_for(&["git", "status"]))
                .await
                .is_approved()
        );
        assert!(matches!(
            approver.request(decision_for(&["github-malware"])).await,
            Decision::DenyWithFeedback(_)
        ));
        assert!(matches!(
            approver
                .request(decision_for(&["git", "status;", "rm", "-rf", "/"]))
                .await,
            Decision::DenyWithFeedback(_)
        ));
        for raw in [
            "git status\nrm -rf /",
            "git status\twhoami",
            "git $(malicious)",
            "git `malicious`",
            "git status > stolen",
            "git status | malicious",
            "git \"status\"",
            "git status\\; malicious",
        ] {
            assert!(
                matches!(
                    approver.request(decision_for(&[raw])).await,
                    Decision::DenyWithFeedback(_)
                ),
                "unsafe shell source was approved: {raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn write_allow_rejects_parent_traversal() {
        use grokforge_protocol::ApprovalId;

        let approver = AutoApprover::new(vec![AllowRule::Write(PathBuf::from("/proj"))]);
        let request = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::WriteFile {
                path: PathBuf::from("/proj/../outside"),
            },
            reason: "test".into(),
        };
        assert!(matches!(
            approver.request(request).await,
            Decision::DenyWithFeedback(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_allow_rejects_symlinked_parent_escape() {
        use grokforge_protocol::ApprovalId;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, workspace.join("link")).expect("symlink");

        let request = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::WriteFile {
                path: workspace.join("link/escaped.txt"),
            },
            reason: "test".into(),
        };
        let decision = AutoApprover::new(vec![AllowRule::Write(workspace)])
            .request(request)
            .await;
        assert!(matches!(decision, Decision::DenyWithFeedback(_)));
    }

    #[tokio::test]
    async fn command_prefix_does_not_grant_sandbox_escape() {
        use grokforge_protocol::{ApprovalId, DenialClass};

        let kind = ApprovalKind::ExecCommand {
            command: vec!["cargo".into(), "test".into()],
            cwd: PathBuf::from("/proj"),
            sandbox: SandboxMode::WorkspaceWrite,
            escalation_of: Some(DenialClass::FsWrite),
        };
        let request = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind,
            reason: "retry".into(),
        };
        let decision = AutoApprover::new(vec![AllowRule::CmdPrefix("cargo".into())])
            .request(request)
            .await;
        assert!(matches!(decision, Decision::DenyWithFeedback(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_grant_cannot_be_retargeted_with_a_symlink() {
        use grokforge_protocol::ApprovalId;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let granted = workspace.path().join("allowed");
        std::fs::create_dir(&granted).unwrap();
        let approver = AutoApprover::new(vec![AllowRule::Write(granted.clone())]);
        std::fs::remove_dir(&granted).unwrap();
        std::os::unix::fs::symlink(outside.path(), &granted).unwrap();
        let request = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::WriteFile {
                path: granted.join("escaped.txt"),
            },
            reason: "test".into(),
        };
        assert!(matches!(
            approver.request(request).await,
            Decision::DenyWithFeedback(_)
        ));
    }

    #[tokio::test]
    async fn write_rule_does_not_grant_sandbox_escalation() {
        use grokforge_protocol::{ApprovalId, DenialClass};

        let request = ApprovalRequest {
            id: ApprovalId::new(),
            call_id: None,
            kind: ApprovalKind::SandboxEscalation {
                original: Box::new(ApprovalKind::WriteFile {
                    path: PathBuf::from("/proj/file"),
                }),
                denial: DenialClass::FsWrite,
            },
            reason: "retry".into(),
        };
        let decision = AutoApprover::new(vec![AllowRule::Write(PathBuf::from("/proj"))])
            .request(request)
            .await;
        assert!(matches!(decision, Decision::DenyWithFeedback(_)));
    }
}
