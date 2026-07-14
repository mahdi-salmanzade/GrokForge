//! Safe project-local slash-command discovery.
//!
//! Each `.grokforge/commands/<name>.md` file is a prompt template. The TUI expands `/name args`
//! into the template plus the user's arguments, then submits it through the normal redaction,
//! rollout, and ledger path.

use std::path::{Path, PathBuf};

const MAX_COMMANDS: usize = 64;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_TOTAL_COMMAND_BYTES: usize = 512 * 1024;
// See the corresponding skill cap: never let filesystem enumeration order choose which commands
// survive a limit.
const MAX_COMMAND_DIRECTORY_ENTRIES: usize = 1_024;
const RESERVED_COMMAND_NAMES: &[&str] = &[
    "clear", "effort", "exit", "help", "memory", "model", "plan", "q", "quit", "skills", "tools",
    "undo",
];

/// A project prompt command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDoc {
    pub name: String,
    pub path: PathBuf,
    pub template: String,
}

/// Discover direct `.md` children in deterministic command-name order.
#[must_use]
pub fn discover(workspace_root: &Path) -> Vec<CommandDoc> {
    let commands_root = workspace_root.join(".grokforge/commands");
    let Ok(canonical_workspace) = std::fs::canonicalize(workspace_root) else {
        return Vec::new();
    };
    let canonical_commands_root = canonical_workspace.join(".grokforge/commands");
    let Ok((entries, truncated)) = crate::path_safety::list_workspace_dir(
        &canonical_workspace,
        &canonical_commands_root,
        MAX_COMMAND_DIRECTORY_ENTRIES + 1,
    ) else {
        return Vec::new();
    };
    if truncated {
        tracing::warn!(
            path = %commands_root.display(),
            "ignoring commands directory with more than {MAX_COMMAND_DIRECTORY_ENTRIES} entries"
        );
        return Vec::new();
    }

    let mut candidates: Vec<(String, String)> = entries
        .into_iter()
        .filter_map(|(file_name, _)| {
            let name = file_name.strip_suffix(".md")?.to_string();
            valid_command_name(&name).then_some((name, file_name))
        })
        .collect();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    if candidates.len() > MAX_COMMANDS {
        tracing::warn!(
            path = %commands_root.display(),
            "command discovery reached the {MAX_COMMANDS}-command limit"
        );
    }
    candidates.truncate(MAX_COMMANDS);

    let mut commands = Vec::new();
    let mut total_bytes = 0usize;
    for (name, file_name) in candidates {
        let path = commands_root.join(file_name);
        let Ok((template, truncated)) = crate::path_safety::read_workspace_context_text(
            workspace_root,
            &path,
            MAX_COMMAND_BYTES,
        ) else {
            tracing::warn!(path = %path.display(), "ignoring unreadable or linked command");
            continue;
        };
        if truncated {
            tracing::warn!(
                path = %path.display(),
                "ignoring command larger than {MAX_COMMAND_BYTES} bytes"
            );
            continue;
        }
        if template.trim().is_empty() {
            continue;
        }
        let Some(next_total) = total_bytes.checked_add(template.len()) else {
            break;
        };
        if next_total > MAX_TOTAL_COMMAND_BYTES {
            tracing::warn!(
                "command discovery reached the {MAX_TOTAL_COMMAND_BYTES}-byte aggregate limit"
            );
            break;
        }
        total_bytes = next_total;
        commands.push(CommandDoc {
            name,
            path,
            template,
        });
    }
    commands
}

/// Expand a project command for submission to the ordinary user-input path.
#[must_use]
pub fn expand(command: &CommandDoc, arguments: &str) -> String {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        format!(
            "[project command /{}]\n\n{}",
            command.name,
            command.template.trim()
        )
    } else {
        format!(
            "[project command /{}]\n\n{}\n\n[arguments]\n{}",
            command.name,
            command.template.trim(),
            arguments
        )
    }
}

fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !RESERVED_COMMAND_NAMES.contains(&name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[cfg(unix)]
    #[test]
    fn discovers_and_expands_commands_in_name_order() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("review.md"), "Review the current diff.").unwrap();
        std::fs::write(root.join("build.md"), "Run the focused build.").unwrap();
        std::fs::write(root.join("ignored.txt"), "ignore me").unwrap();

        let commands = discover(workspace.path());
        let names: Vec<&str> = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, ["build", "review"]);
        assert_eq!(
            expand(&commands[1], "src/lib.rs"),
            "[project command /review]\n\nReview the current diff.\n\n[arguments]\nsrc/lib.rs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignores_linked_command() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret").unwrap();
        let root = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&root).unwrap();
        symlink(outside.path(), root.join("leak.md")).unwrap();

        assert!(discover(workspace.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_hard_linked_command() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret").unwrap();
        let root = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::hard_link(outside.path(), root.join("leak.md")).unwrap();

        assert!(discover(workspace.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn caps_are_applied_after_deterministic_name_sorting() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&root).unwrap();
        for index in (0..MAX_COMMANDS + 8).rev() {
            std::fs::write(
                root.join(format!("command-{index:03}.md")),
                "Run a bounded command.",
            )
            .unwrap();
        }

        let commands = discover(workspace.path());
        assert_eq!(commands.len(), MAX_COMMANDS);
        assert_eq!(commands.first().unwrap().name, "command-000");
        assert_eq!(commands.last().unwrap().name, "command-063");
    }

    #[cfg(unix)]
    #[test]
    fn aggregate_command_bytes_are_bounded() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..9 {
            std::fs::write(
                root.join(format!("command-{index:03}.md")),
                vec![b'x'; MAX_COMMAND_BYTES],
            )
            .unwrap();
        }

        let commands = discover(workspace.path());
        assert_eq!(commands.len(), MAX_TOTAL_COMMAND_BYTES / MAX_COMMAND_BYTES);
    }

    #[test]
    fn validates_names_and_empty_arguments() {
        assert!(valid_command_name("fix-tests"));
        assert!(!valid_command_name("../escape"));
        assert!(!valid_command_name("help"));
        assert!(!valid_command_name("tools"));
        let command = CommandDoc {
            name: "review".to_string(),
            path: PathBuf::from("review.md"),
            template: "  Review carefully.  ".to_string(),
        };
        assert_eq!(
            expand(&command, "  "),
            "[project command /review]\n\nReview carefully."
        );
    }
}
