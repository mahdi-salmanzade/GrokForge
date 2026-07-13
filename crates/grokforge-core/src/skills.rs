//! Project-local Agent Skills discovery.
//!
//! A skill lives at `.grokforge/skills/<name>/SKILL.md`. Skills are automatic outbound
//! context, so discovery uses the same no-symlink, no-hard-link reader as `AGENTS.md` and keeps
//! both the number and total bytes bounded.

use std::path::{Path, PathBuf};

const MAX_SKILLS: usize = 32;
const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_TOTAL_SKILL_BYTES: usize = 512 * 1024;
// Directory iteration order is filesystem-dependent. Scan a bounded superset, then sort it; if
// even that scan is exhausted, fail closed instead of advertising an arbitrary subset.
const MAX_SKILL_DIRECTORY_ENTRIES: usize = 1_024;
const MAX_DESCRIPTION_CHARS: usize = 300;
const DEFAULT_DESCRIPTION: &str = "Project-provided workflow instructions.";

/// A project skill whose instructions apply to the current workspace.
#[derive(Debug, Clone)]
pub struct SkillDoc {
    pub name: String,
    pub path: PathBuf,
    /// A short catalog description. Full instructions stay local until the model chooses the
    /// skill and reads `path` with the ordinary ledgered `read_file` tool.
    pub description: String,
}

/// Discover bounded, project-local skills in deterministic name order.
#[must_use]
pub fn discover(workspace_root: &Path) -> Vec<SkillDoc> {
    let skills_root = workspace_root.join(".grokforge/skills");
    // Align macOS' lexical `/var` temporary paths with their canonical `/private/var` identity
    // before the descriptor-relative directory listing. Individual skill reads below still use
    // the original lexical path and refuse every symlink component.
    let Ok(canonical_workspace) = std::fs::canonicalize(workspace_root) else {
        return Vec::new();
    };
    let canonical_skills_root = canonical_workspace.join(".grokforge/skills");
    let Ok((entries, truncated)) = crate::path_safety::list_workspace_dir(
        &canonical_workspace,
        &canonical_skills_root,
        MAX_SKILL_DIRECTORY_ENTRIES + 1,
    ) else {
        return Vec::new();
    };
    if truncated {
        tracing::warn!(
            path = %skills_root.display(),
            "ignoring skills directory with more than {MAX_SKILL_DIRECTORY_ENTRIES} entries"
        );
        return Vec::new();
    }

    // Some filesystems report an unknown directory-entry type. Treat safe entry names as
    // candidates and let the descriptor-relative `SKILL.md` open below prove the shape without
    // following links.
    let mut names: Vec<String> = entries
        .into_iter()
        .filter_map(|(name, _)| valid_skill_name(&name).then_some(name))
        .collect();
    names.sort();
    if names.len() > MAX_SKILLS {
        tracing::warn!(
            path = %skills_root.display(),
            "skill discovery reached the {MAX_SKILLS}-skill limit"
        );
    }
    names.truncate(MAX_SKILLS);

    let mut skills = Vec::new();
    let mut total_bytes = 0usize;
    for name in names {
        let path = skills_root.join(&name).join("SKILL.md");
        let Ok((content, truncated)) =
            crate::path_safety::read_workspace_context_text(workspace_root, &path, MAX_SKILL_BYTES)
        else {
            tracing::warn!(path = %path.display(), "ignoring unreadable or linked skill");
            continue;
        };
        if truncated {
            tracing::warn!(
                path = %path.display(),
                "ignoring skill larger than {MAX_SKILL_BYTES} bytes"
            );
            continue;
        }
        let Some(next_total) = total_bytes.checked_add(content.len()) else {
            break;
        };
        if next_total > MAX_TOTAL_SKILL_BYTES {
            tracing::warn!(
                "skill discovery reached the {MAX_TOTAL_SKILL_BYTES}-byte aggregate limit"
            );
            break;
        }
        total_bytes = next_total;
        skills.push(SkillDoc {
            name,
            path,
            description: skill_description(&content),
        });
    }
    skills
}

fn skill_description(content: &str) -> String {
    // A UTF-8 BOM is common in files created by Windows editors and is not semantic content.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().is_none_or(|line| line.trim() != "---") {
        // Only explicitly declared catalog metadata is automatic outbound context. Never infer a
        // description from the instruction body: it stays local until a deliberate `read_file`.
        return DEFAULT_DESCRIPTION.to_string();
    }
    let Some(frontmatter_end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
    else {
        // Do not mistake malformed frontmatter fields for a public catalog description.
        return DEFAULT_DESCRIPTION.to_string();
    };
    frontmatter_description(&lines[1..frontmatter_end])
        .unwrap_or_else(|| DEFAULT_DESCRIPTION.to_string())
}

/// Extract the small subset of YAML frontmatter needed for the skill catalog. Skill bodies are
/// intentionally not parsed as YAML, and malformed/unsupported values use a generic catalog
/// description so instruction-body prose never becomes automatic outbound context.
fn frontmatter_description(frontmatter: &[&str]) -> Option<String> {
    for (index, line) in frontmatter.iter().enumerate() {
        // `description` is a top-level Agent Skills field. Requiring it at column zero prevents
        // an indented block line containing `description:` from being mistaken for the field.
        let Some(value) = line.strip_prefix("description:") else {
            continue;
        };
        let value = value.trim();
        if !matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            return clean_description(value);
        }

        let mut block = String::new();
        for continuation in &frontmatter[index.saturating_add(1)..] {
            if !continuation.is_empty()
                && !continuation.chars().next().is_some_and(char::is_whitespace)
            {
                break;
            }
            let continuation = continuation.trim();
            if !continuation.is_empty() {
                if !block.is_empty() {
                    block.push(' ');
                }
                block.push_str(continuation);
            }
        }
        return clean_description(&block);
    }
    None
}

fn clean_description(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || matches!(value, "null" | "~") {
        return None;
    }

    // JSON string decoding is a safe approximation for YAML's double-quoted scalar syntax. For
    // an invalid escape, retain the inner text rather than rejecting an otherwise useful skill.
    let decoded;
    let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        decoded = serde_json::from_str::<String>(value)
            .unwrap_or_else(|_| value[1..value.len().saturating_sub(1)].to_string());
        decoded.as_str()
    } else if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        decoded = value[1..value.len().saturating_sub(1)].replace("''", "'");
        decoded.as_str()
    } else {
        value
    };

    let mut output = String::new();
    let mut pending_space = false;
    let mut characters = 0usize;
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space && characters.saturating_add(1) >= MAX_DESCRIPTION_CHARS {
                break;
            }
            if pending_space {
                output.push(' ');
                characters = characters.saturating_add(1);
            }
            if characters >= MAX_DESCRIPTION_CHARS {
                break;
            }
            output.push(character);
            characters = characters.saturating_add(1);
            pending_space = false;
        }
    }
    (!output.is_empty()).then_some(output)
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
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
    fn discovers_project_skills_in_name_order() {
        let workspace = tempfile::tempdir().unwrap();
        for (name, body) in [
            (
                "review",
                "---\nname: review\ndescription: Review carefully\n---\n# Review",
            ),
            ("build", "# Build\n\nBuild safely"),
        ] {
            let directory = workspace.path().join(".grokforge/skills").join(name);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("SKILL.md"), body).unwrap();
        }

        let skills = discover(workspace.path());
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["build", "review"]);
        assert_eq!(skills[0].description, DEFAULT_DESCRIPTION);
        assert_eq!(skills[1].description, "Review carefully");
    }

    #[cfg(unix)]
    #[test]
    fn ignores_linked_skill_files_and_directories() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("SKILL.md"), "outside secret").unwrap();
        let root = workspace.path().join(".grokforge/skills");
        std::fs::create_dir_all(&root).unwrap();
        symlink(outside.path(), root.join("linked-directory")).unwrap();

        let regular = root.join("linked-file");
        std::fs::create_dir_all(&regular).unwrap();
        symlink(outside.path().join("SKILL.md"), regular.join("SKILL.md")).unwrap();

        assert!(discover(workspace.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_hard_linked_skill_file() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret").unwrap();
        let skill = workspace.path().join(".grokforge/skills/linked-file");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::hard_link(outside.path(), skill.join("SKILL.md")).unwrap();

        assert!(discover(workspace.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn caps_are_applied_after_deterministic_name_sorting() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".grokforge/skills");
        for index in (0..MAX_SKILLS + 8).rev() {
            let skill = root.join(format!("skill-{index:03}"));
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "A bounded skill").unwrap();
        }

        let skills = discover(workspace.path());
        assert_eq!(skills.len(), MAX_SKILLS);
        assert_eq!(skills.first().unwrap().name, "skill-000");
        assert_eq!(skills.last().unwrap().name, "skill-031");
    }

    #[cfg(unix)]
    #[test]
    fn aggregate_skill_bytes_are_bounded() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join(".grokforge/skills");
        for index in 0..9 {
            let skill = root.join(format!("skill-{index:03}"));
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), vec![b'x'; MAX_SKILL_BYTES]).unwrap();
        }

        let skills = discover(workspace.path());
        assert_eq!(skills.len(), MAX_TOTAL_SKILL_BYTES / MAX_SKILL_BYTES);
    }

    #[test]
    fn parses_bounded_frontmatter_descriptions() {
        assert_eq!(
            skill_description(
                "\u{feff}---\r\nname: review\r\ndescription: >-\r\n  Review changes\r\n  carefully.\r\n---\r\n# Review"
            ),
            "Review changes carefully."
        );
        assert_eq!(
            skill_description("---\ndescription: \"Review \\\"quoted\\\" text\"\n---\n# Review"),
            "Review \"quoted\" text"
        );
        assert_eq!(
            skill_description("---\ndescription: 'Don''t rush'\n---\n# Review"),
            "Don't rush"
        );
        assert_eq!(
            skill_description("---\ndescription: not body prose\n# Missing delimiter"),
            DEFAULT_DESCRIPTION
        );
        assert_eq!(
            skill_description("# Internal workflow\n\nDo not advertise this body text."),
            DEFAULT_DESCRIPTION
        );

        let long = format!("{}   tail", "x".repeat(MAX_DESCRIPTION_CHARS + 10));
        let description = clean_description(&long).unwrap();
        assert_eq!(description.chars().count(), MAX_DESCRIPTION_CHARS);
        assert!(!description.ends_with(' '));
    }

    #[test]
    fn rejects_unsafe_names() {
        assert!(valid_skill_name("rust-review"));
        assert!(valid_skill_name("release_2"));
        assert!(!valid_skill_name("../escape"));
        assert!(!valid_skill_name("has space"));
    }
}
