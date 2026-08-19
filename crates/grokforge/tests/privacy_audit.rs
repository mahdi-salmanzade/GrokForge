//! ADR 0004 source audit: the privacy claim is exactly what this test asserts.
//!
//! Product telemetry must not exist as a dependency, an SDK import/path, or a
//! hardcoded analytics ingest URL. `tracing` / `tracing-subscriber` are logging
//! and `reqwest` is the HTTP client — they are allowed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

const SKIP_DIR_NAMES: &[&str] = &["target", ".git", "graphify-out", "node_modules"];
const MIN_CARGO_TOML_FILES: usize = 10;

/// Logging / HTTP — not product telemetry.
const ALLOWED_DEPENDENCIES: &[&str] = &["tracing", "tracing-subscriber", "reqwest"];

/// Exact Cargo package names that are product telemetry.
const FORBIDDEN_DEPENDENCIES: &[&str] = &[
    "sentry",
    "sentry-tracing",
    "opentelemetry",
    "tracing-opentelemetry",
    "mixpanel",
    "posthog",
    "segment",
    "amplitude",
    "telemetry",
];

/// Prefix families for renamed / sibling SDK crates (`sentry-anyhow`, `opentelemetry-otlp`).
/// `segment` is exact-only so `unicode-segmentation` stays allowed.
const FORBIDDEN_DEP_PREFIXES: &[&str] = &[
    "sentry-",
    "sentry_",
    "opentelemetry-",
    "opentelemetry_",
    "tracing-opentelemetry",
    "mixpanel-",
    "mixpanel_",
    "posthog-",
    "posthog_",
    "amplitude-",
    "amplitude_",
    "telemetry-",
    "telemetry_",
];

const FORBIDDEN_DEP_SUFFIXES: &[&str] = &[
    "-telemetry",
    "_telemetry",
    "-opentelemetry",
    "_opentelemetry",
    "-sentry",
    "_sentry",
];

/// Rust crate-root idents (hyphens become underscores). `segment` is exact-only.
const FORBIDDEN_RUST_IDENTS: &[&str] = &[
    "sentry",
    "sentry_tracing",
    "opentelemetry",
    "tracing_opentelemetry",
    "mixpanel",
    "posthog",
    "segment",
    "amplitude",
    "telemetry",
];

const FORBIDDEN_RUST_PREFIXES: &[&str] = &[
    "sentry_",
    "opentelemetry_",
    "tracing_opentelemetry_",
    "mixpanel_",
    "posthog_",
    "amplitude_",
    "telemetry_",
];

#[derive(Debug)]
struct Finding {
    path: PathBuf,
    line: usize,
    message: String,
}

impl Finding {
    fn new(path: &Path, line: usize, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            line,
            message: message.into(),
        }
    }

    fn render(&self, root: &Path) -> String {
        format!("{}:{}: {}", rel(root, &self.path), self.line, self.message)
    }
}

#[test]
fn privacy_claim_is_exactly_the_source_audit() {
    let root = workspace_root();
    assert_security_md_anchor(&root);

    let files = collect_source_files(&root);
    let cargo_tomls: Vec<&PathBuf> = files.iter().filter(|path| is_cargo_toml(path)).collect();
    assert!(
        cargo_tomls.len() >= MIN_CARGO_TOML_FILES,
        "source walk found {} Cargo.toml file(s); expected at least {MIN_CARGO_TOML_FILES} \
         so the audit cannot pass on an empty tree",
        cargo_tomls.len()
    );

    let mut findings = Vec::new();
    for path in cargo_tomls {
        findings.extend(audit_cargo_toml(path));
    }
    for path in files.iter().filter(|path| is_rust_file(path)) {
        findings.extend(audit_rust_file(path));
    }

    if !findings.is_empty() {
        let report = findings
            .iter()
            .map(|finding| finding.render(&root))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "ADR 0004 forbids telemetry code paths; found {}:\n{report}",
            findings.len()
        );
    }
}

#[test]
fn matcher_allows_logging_http_and_rejects_telemetry_crates() {
    assert!(!is_forbidden_dependency("tracing"));
    assert!(!is_forbidden_dependency("tracing-subscriber"));
    assert!(!is_forbidden_dependency("reqwest"));
    assert!(!is_forbidden_dependency("unicode-segmentation"));
    assert!(is_forbidden_dependency("sentry"));
    assert!(is_forbidden_dependency("sentry-tracing"));
    assert!(is_forbidden_dependency("opentelemetry-otlp"));
    assert!(is_forbidden_dependency("posthog"));
    assert!(is_forbidden_dependency("telemetry"));
    assert!(is_forbidden_dependency("segment"));

    let sentry = format!("{}::init", rust_ident("sentry"));
    let import = format!("use {}::Client", rust_ident("posthog"));
    assert!(rust_code_uses_forbidden_sdk(&sentry).is_some());
    assert!(rust_code_uses_forbidden_sdk(&import).is_some());
    assert!(rust_code_uses_forbidden_sdk("tracing::info!(\"hi\")").is_none());
    assert!(rust_code_uses_forbidden_sdk("for segment in parts {}").is_none());
    assert!(rust_code_uses_forbidden_sdk("//! GrokForge has no telemetry.").is_none());
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root from CARGO_MANIFEST_DIR/../..")
}

fn assert_security_md_anchor(root: &Path) {
    let path = root.join("SECURITY.md");
    assert!(
        path.is_file(),
        "SECURITY.md must exist at the repository root as the privacy-claim anchor"
    );
    let text = fs::read_to_string(&path).expect("read SECURITY.md");
    assert!(
        text.to_ascii_lowercase().contains("no telemetry"),
        "{} must contain \"no telemetry\" (case-insensitive)",
        rel(root, &path)
    );
}

fn collect_source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_dir(root, &mut files);
    files.sort();
    files
}

fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()))
        .map(|entry| entry.expect("dir entry"))
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let name = entry.file_name();
        let path = entry.path();
        let file_type = entry.file_type().expect("file type");
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if skip_dir_name(&name) {
                continue;
            }
            walk_dir(&path, files);
            continue;
        }
        if file_type.is_file() {
            files.push(path);
        }
    }
}

fn skip_dir_name(name: &OsStr) -> bool {
    SKIP_DIR_NAMES.iter().any(|skip| name == *skip)
}

fn is_cargo_toml(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("Cargo.toml"))
}

fn is_rust_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("rs"))
}

fn is_forbidden_dependency(name: &str) -> bool {
    let name = normalize_toml_key(name);
    if ALLOWED_DEPENDENCIES.contains(&name) {
        return false;
    }
    if FORBIDDEN_DEPENDENCIES.contains(&name) {
        return true;
    }
    FORBIDDEN_DEP_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || FORBIDDEN_DEP_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn normalize_toml_key(name: &str) -> &str {
    name.trim().trim_matches('"').trim_matches('\'')
}

fn is_forbidden_rust_ident(ident: &str) -> bool {
    if matches!(ident, "tracing" | "tracing_subscriber" | "reqwest") {
        return false;
    }
    FORBIDDEN_RUST_IDENTS.contains(&ident)
        || FORBIDDEN_RUST_PREFIXES
            .iter()
            .any(|prefix| ident.starts_with(prefix))
}

fn rust_ident(crate_name: &str) -> String {
    crate_name.replace('-', "_")
}

fn audit_cargo_toml(path: &Path) -> Vec<Finding> {
    let text = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    });
    let mut findings = Vec::new();
    let mut in_dependency_table = false;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = strip_toml_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(header) = toml_table_header(trimmed) {
            if let Some(crate_name) = named_dependency_from_header(header) {
                in_dependency_table = false;
                if is_forbidden_dependency(crate_name) {
                    findings.push(Finding::new(
                        path,
                        line_no,
                        format!("forbidden telemetry dependency `{crate_name}`"),
                    ));
                }
                continue;
            }
            in_dependency_table = is_dependency_table(header);
            continue;
        }
        if !in_dependency_table {
            continue;
        }
        if let Some(crate_name) = toml_dependency_key(trimmed)
            && is_forbidden_dependency(crate_name)
        {
            findings.push(Finding::new(
                path,
                line_no,
                format!("forbidden telemetry dependency `{crate_name}`"),
            ));
        }
    }
    findings
}

fn strip_toml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

fn toml_table_header(line: &str) -> Option<&str> {
    let line = line.trim();
    if !line.starts_with('[') || !line.ends_with(']') {
        return None;
    }
    let inner = line
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_start_matches('[')
        .trim_end_matches(']');
    Some(inner.trim())
}

fn is_dependency_table(header: &str) -> bool {
    matches!(
        header,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    ) || header.ends_with(".dependencies")
        || header.ends_with(".dev-dependencies")
        || header.ends_with(".build-dependencies")
}

fn named_dependency_from_header(header: &str) -> Option<&str> {
    for prefix in ["dependencies.", "dev-dependencies.", "build-dependencies."] {
        if let Some(name) = header.strip_prefix(prefix)
            && is_simple_crate_name(name)
        {
            return Some(name);
        }
    }
    None
}

fn is_simple_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn toml_dependency_key(line: &str) -> Option<&str> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() {
        return None;
    }
    let name = key.split('.').next().unwrap_or(key);
    let name = normalize_toml_key(name);
    if name.is_empty() || !is_simple_crate_name(name) {
        None
    } else {
        Some(name)
    }
}

fn audit_rust_file(path: &Path) -> Vec<Finding> {
    let text = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    });
    let mut findings = Vec::new();
    let mut in_block_comment = false;
    let needles = forbidden_ingest_needles();

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let lowered = raw_line.to_ascii_lowercase();
        for needle in &needles {
            if lowered.contains(needle) {
                findings.push(Finding::new(
                    path,
                    line_no,
                    format!("hardcoded product-analytics ingest URL `{needle}`"),
                ));
            }
        }

        let visible = visible_rust_code(raw_line, &mut in_block_comment);
        if let Some(ident) = rust_code_uses_forbidden_sdk(&visible) {
            findings.push(Finding::new(
                path,
                line_no,
                format!("telemetry SDK use `{ident}`"),
            ));
        }
    }
    findings
}

/// Hosts are assembled so this audit file does not contain the needles it searches for.
fn forbidden_ingest_needles() -> Vec<String> {
    vec![
        concat!("api.", "segment.io").to_ascii_lowercase(),
        concat!("cdn.", "segment.com").to_ascii_lowercase(),
        concat!("api.", "segment.com").to_ascii_lowercase(),
        concat!("ingest.", "sentry.io").to_ascii_lowercase(),
        concat!("sentry.io/", "api").to_ascii_lowercase(),
        concat!("sentry-cdn.", "com").to_ascii_lowercase(),
        concat!("decide.", "posthog.com").to_ascii_lowercase(),
        concat!("app.", "posthog.com").to_ascii_lowercase(),
        concat!("i.posthog.", "com").to_ascii_lowercase(),
        concat!("api.", "mixpanel.com").to_ascii_lowercase(),
        concat!("api-js.", "mixpanel.com").to_ascii_lowercase(),
        concat!("decide.", "mixpanel.com").to_ascii_lowercase(),
        concat!("api.", "amplitude.com").to_ascii_lowercase(),
        concat!("api2.", "amplitude.com").to_ascii_lowercase(),
        concat!("api.eu.", "amplitude.com").to_ascii_lowercase(),
        concat!("cdn.", "amplitude.com").to_ascii_lowercase(),
    ]
}

fn visible_rust_code(line: &str, in_block_comment: &mut bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if *in_block_comment {
            if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                *in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            *in_block_comment = true;
            i += 2;
            continue;
        }
        if chars[i] == '/'
            && i + 1 < chars.len()
            && chars[i + 1] == '/'
            && !preceded_by_colon(&chars, i)
        {
            break;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn preceded_by_colon(chars: &[char], i: usize) -> bool {
    i > 0 && chars[i - 1] == ':'
}

fn rust_code_uses_forbidden_sdk(code: &str) -> Option<String> {
    // Doc / line comments are stripped before this is called. A leftover `//`
    // line is ignored so "no telemetry" documentation cannot match.
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") {
        return None;
    }

    let mut prev: Option<&str> = None;
    let mut prev2: Option<&str> = None;
    for (ident, after) in rust_idents(code) {
        let used_as_path = after.starts_with("::");
        let used_as_import = matches!(prev, Some("use"));
        let used_as_extern = matches!((prev2, prev), (Some("extern"), Some("crate")));
        if (used_as_path || used_as_import || used_as_extern) && is_forbidden_rust_ident(ident) {
            return Some(ident.to_string());
        }
        prev2 = prev;
        prev = Some(ident);
    }
    None
}

fn rust_idents(code: &str) -> Vec<(&str, &str)> {
    let bytes = code.as_bytes();
    let mut idents = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let ident = &code[start..i];
            let after = code[i..].trim_start();
            idents.push((ident, after));
        } else {
            i += 1;
        }
    }
    idents
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
