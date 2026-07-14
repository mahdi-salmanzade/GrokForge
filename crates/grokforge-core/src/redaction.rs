//! Secret redaction. Runs over every source that can carry bytes to the model — file reads,
//! tool output, and user-typed/pasted input — so a leaked key never reaches a request body
//! even if the sandbox and blocked-glob layers are bypassed (ADR 0003, 0004).

use regex::Regex;
use std::sync::OnceLock;

/// A named redaction rule.
struct Rule {
    name: &'static str,
    re: Regex,
    /// Which capture group holds the secret to mask (0 = the whole match).
    group: usize,
}

fn compile_rules() -> Result<Vec<Rule>, regex::Error> {
    // Note: these are conservative and may miss exotic formats; the blocked-glob and sandbox
    // layers are the other two lines of defense.
    Ok(vec![
        Rule {
            name: "private-key",
            re: Regex::new(
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            )?,
            group: 0,
        },
        Rule {
            // Environment assignments such as AWS_SECRET_ACCESS_KEY=..., including
            // quoted values and punctuation commonly found in generated credentials.
            name: "environment-secret",
            re: Regex::new(
                r#"(?im)\b[A-Z_][A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD)[A-Z0-9_]*\s*=\s*(\"[^\"\r\n]{8,}\"|'[^'\r\n]{8,}'|[^\s#\r\n]{8,})"#,
            )?,
            group: 1,
        },
        Rule {
            name: "xai-key",
            re: Regex::new(r"xai-[A-Za-z0-9]{16,}")?,
            group: 0,
        },
        Rule {
            name: "aws-access-key",
            re: Regex::new(r"AKIA[0-9A-Z]{16}")?,
            group: 0,
        },
        Rule {
            name: "github-token",
            re: Regex::new(r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})")?,
            group: 0,
        },
        Rule {
            name: "slack-token",
            re: Regex::new(r"xox[baprs]-[A-Za-z0-9-]{10,}")?,
            group: 0,
        },
        Rule {
            name: "stripe-key",
            re: Regex::new(r"sk_(?:live|test)_[A-Za-z0-9]{10,}")?,
            group: 0,
        },
        Rule {
            name: "google-api-key",
            re: Regex::new(r"AIza[0-9A-Za-z_-]{20,}")?,
            group: 0,
        },
        Rule {
            name: "basic-auth",
            re: Regex::new(r"(?i)\bbasic\s+([A-Za-z0-9+/]{8,}={0,2})")?,
            group: 1,
        },
        Rule {
            name: "bearer-token",
            re: Regex::new(r"(?i)\bbearer\s+([A-Za-z0-9._\-]{20,})")?,
            group: 1,
        },
        // KEY=secret / api_key: "secret" style assignments with a long value.
        Rule {
            name: "assigned-secret",
            re: Regex::new(
                r#"(?i)(?:api[_-]?key|secret|token|password|passwd)\s*[:=]\s*["']?([^\s"'#\[]+)["']?"#,
            )?,
            group: 1,
        },
    ])
}

fn rules() -> Result<&'static [Rule], &'static regex::Error> {
    static RULES: OnceLock<Result<Vec<Rule>, regex::Error>> = OnceLock::new();
    RULES.get_or_init(compile_rules).as_deref()
}

/// Applies redaction rules to text.
#[derive(Debug, Default, Clone, Copy)]
pub struct Redactor;

/// The result of redacting some text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub count: usize,
}

impl Redactor {
    /// Replace every matched secret with `[REDACTED:<rule>]`, returning the count.
    #[must_use]
    pub fn apply(text: &str) -> Redacted {
        let Ok(rules) = rules() else {
            // The patterns are literals, so this can only indicate a programmer/build defect.
            // Redaction is a privacy boundary: fail closed instead of forwarding the input.
            return Redacted {
                text: "[REDACTED:redactor-unavailable]".to_string(),
                count: usize::from(!text.is_empty()),
            };
        };
        let mut out = text.to_string();
        let mut count = 0usize;
        for rule in rules {
            // Rebuild once per rule. Repeated `replace_range` shifts the remaining suffix for
            // every dense match and becomes quadratic on a large pasted environment dump.
            let marker = format!("[REDACTED:{}]", rule.name);
            let mut rebuilt = String::with_capacity(out.len().min(1024 * 1024));
            let mut cursor = 0usize;
            let mut matched = false;
            for caps in rule.re.captures_iter(&out) {
                if let Some(m) = caps.get(rule.group) {
                    rebuilt.push_str(&out[cursor..m.start()]);
                    rebuilt.push_str(&marker);
                    cursor = m.end();
                    count = count.saturating_add(1);
                    matched = true;
                }
            }
            if matched {
                rebuilt.push_str(&out[cursor..]);
                out = rebuilt;
            }
        }
        Redacted { text: out, count }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_xai_key() {
        let r = Redactor::apply("token is xai-ABCDEF0123456789XYZ done");
        assert!(!r.text.contains("xai-ABCDEF0123456789XYZ"));
        assert!(r.text.contains("[REDACTED:xai-key]"));
        assert_eq!(r.count, 1);
    }

    #[test]
    fn redacts_env_style_assignment() {
        let r = Redactor::apply("SECRET=hunter2_supersecret_value_here");
        assert!(!r.text.contains("hunter2_supersecret_value_here"));
        assert_eq!(r.count, 1);
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        let r = Redactor::apply(pem);
        assert!(!r.text.contains("MIIabc"));
        assert_eq!(r.count, 1);
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let r = Redactor::apply("the quick brown fox jumps over the lazy dog");
        assert_eq!(r.count, 0);
        assert_eq!(r.text, "the quick brown fox jumps over the lazy dog");
    }

    #[test]
    fn redacts_multiple_secrets() {
        let r = Redactor::apply("a xai-AAAAAAAAAAAAAAAA and AKIAABCDEFGHIJKLMNOP end");
        assert_eq!(r.count, 2);
    }

    #[test]
    fn redacts_prefixed_environment_secret_with_punctuation() {
        let r = Redactor::apply(
            "AWS_SECRET_ACCESS_KEY='abc/DEF+123$456!'\nGITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz",
        );
        assert!(!r.text.contains("abc/DEF+123$456!"));
        assert!(!r.text.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(r.count, 2);
    }

    #[test]
    fn redacts_short_named_secret_and_common_provider_formats() {
        let r = Redactor::apply(
            "PASSWORD=abc\nAuthorization: Basic dXNlcjpwYXNz\n\
             github_pat_abcdefghijklmnopqrstuvwxyz\n\
             xoxb-1234567890-abcdefghij\n\
             sk_live_abcdefghijklmnop\n\
             AIzaabcdefghijklmnopqrstuvwxyz",
        );
        for leaked in [
            "PASSWORD=abc",
            "dXNlcjpwYXNz",
            "github_pat_abcdefghijklmnopqrstuvwxyz",
            "xoxb-1234567890-abcdefghij",
            "sk_live_abcdefghijklmnop",
            "AIzaabcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(!r.text.contains(leaked), "leaked {leaked}");
        }
        assert!(r.count >= 6);
    }

    #[test]
    fn dense_eight_mib_environment_dump_is_redacted_linearly() {
        let assignment = "TOKEN=abcdefgh\n";
        let repetitions = (8 * 1024 * 1024) / assignment.len();
        let input = assignment.repeat(repetitions);
        let redacted = Redactor::apply(&input);
        assert_eq!(redacted.count, repetitions);
        assert!(!redacted.text.contains("abcdefgh"));
    }
}
