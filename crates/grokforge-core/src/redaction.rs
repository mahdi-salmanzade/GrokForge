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

// The patterns are static, hand-verified literals; a compile-time-constant regex cannot fail
// to parse, so `expect` here is a genuine invariant, not fallible input handling.
#[allow(clippy::expect_used)]
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES
        .get_or_init(|| {
            // Note: these are conservative and may miss exotic formats; the blocked-glob and
            // sandbox layers are the other two lines of defense.
            vec![
                Rule {
                    name: "private-key",
                    re: Regex::new(
                        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
                    )
                    .expect("valid regex"),
                    group: 0,
                },
                Rule {
                    name: "xai-key",
                    re: Regex::new(r"xai-[A-Za-z0-9]{16,}").expect("valid regex"),
                    group: 0,
                },
                Rule {
                    name: "aws-access-key",
                    re: Regex::new(r"AKIA[0-9A-Z]{16}").expect("valid regex"),
                    group: 0,
                },
                Rule {
                    name: "bearer-token",
                    re: Regex::new(r"(?i)bearer\s+([A-Za-z0-9._\-]{20,})").expect("valid regex"),
                    group: 1,
                },
                // KEY=secret / api_key: "secret" style assignments with a long value.
                Rule {
                    name: "assigned-secret",
                    re: Regex::new(
                        r#"(?i)(?:api[_-]?key|secret|token|password|passwd)\s*[:=]\s*["']?([A-Za-z0-9_\-./+]{12,})["']?"#,
                    )
                    .expect("valid regex"),
                    group: 1,
                },
            ]
        })
        .as_slice()
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
        let mut out = text.to_string();
        let mut count = 0usize;
        for rule in rules() {
            // Collect the exact byte spans to replace, then apply right-to-left so earlier
            // offsets stay valid.
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for caps in rule.re.captures_iter(&out) {
                if let Some(m) = caps.get(rule.group) {
                    spans.push((m.start(), m.end()));
                }
            }
            if spans.is_empty() {
                continue;
            }
            let marker = format!("[REDACTED:{}]", rule.name);
            for (start, end) in spans.into_iter().rev() {
                out.replace_range(start..end, &marker);
                count += 1;
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
}
