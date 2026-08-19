//! Deterministic, bounded, source-oriented repository maps.
//!
//! Discovery respects project `.gitignore`/`.ignore` files, omits hidden entries, never follows
//! symlinks, and does not consult machine-global ignore configuration. File contents are opened
//! relative to the canonical repository root with `O_NOFOLLOW` on Unix; multiply-linked files are
//! skipped as well. This keeps silently discovered context from becoming a path traversal or hard
//! link disclosure primitive.

use std::cmp::Reverse;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

/// Hard ceiling for files whose contents may be inspected in one map.
pub const HARD_MAX_FILES: usize = 2_000;
/// Hard ceiling for filesystem entries considered in one map.
pub const HARD_MAX_WALK_ENTRIES: usize = 50_000;
/// Hard traversal-depth ceiling, relative to the repository root.
pub const HARD_MAX_DEPTH: usize = 32;
/// Hard ceiling for bytes read from any one candidate file.
pub const HARD_MAX_FILE_BYTES: usize = 512 * 1024;
/// Hard ceiling for all file bytes read by one map.
pub const HARD_MAX_TOTAL_READ_BYTES: usize = 8 * 1024 * 1024;
/// Hard ceiling for rendered map bytes.
pub const HARD_MAX_OUTPUT_BYTES: usize = 128 * 1024;
/// Hard ceiling for symbols retained from a file.
pub const HARD_MAX_SYMBOLS_PER_FILE: usize = 128;

/// Resource limits for one repository map. Values are always clamped to the hard ceilings above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoMapLimits {
    pub max_files: usize,
    pub max_walk_entries: usize,
    pub max_depth: usize,
    pub max_file_bytes: usize,
    pub max_total_read_bytes: usize,
    pub max_output_bytes: usize,
    pub max_symbols_per_file: usize,
}

impl Default for RepoMapLimits {
    fn default() -> Self {
        Self {
            max_files: 1_000,
            max_walk_entries: 20_000,
            max_depth: 16,
            max_file_bytes: 128 * 1024,
            max_total_read_bytes: 2 * 1024 * 1024,
            max_output_bytes: 48 * 1024,
            max_symbols_per_file: 48,
        }
    }
}

impl RepoMapLimits {
    fn bounded(self) -> Self {
        Self {
            max_files: self.max_files.clamp(1, HARD_MAX_FILES),
            max_walk_entries: self.max_walk_entries.clamp(1, HARD_MAX_WALK_ENTRIES),
            max_depth: self.max_depth.clamp(1, HARD_MAX_DEPTH),
            max_file_bytes: self.max_file_bytes.clamp(1, HARD_MAX_FILE_BYTES),
            max_total_read_bytes: self
                .max_total_read_bytes
                .clamp(1, HARD_MAX_TOTAL_READ_BYTES),
            max_output_bytes: self.max_output_bytes.clamp(256, HARD_MAX_OUTPUT_BYTES),
            max_symbols_per_file: self
                .max_symbols_per_file
                .clamp(1, HARD_MAX_SYMBOLS_PER_FILE),
        }
    }
}

/// Inputs to repository-map construction.
#[derive(Debug, Clone, Default)]
pub struct RepoMapOptions {
    pub limits: RepoMapLimits,
    /// Optional free-text relevance query. It only changes local ranking; it is never sent away.
    pub query: Option<String>,
    /// Additional deny globs, normally copied from the active secrets policy.
    pub excluded_globs: Vec<String>,
}

/// Summary counters describing what the bounded builder did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Each bound is reported independently for auditing.
pub struct RepoMapStats {
    pub walked_entries: usize,
    pub candidate_files: usize,
    pub mapped_files: usize,
    pub mapped_symbols: usize,
    pub bytes_read: usize,
    pub skipped_unsafe: usize,
    pub skipped_unreadable: usize,
    pub walk_truncated: bool,
    pub files_truncated: bool,
    pub read_truncated: bool,
    pub output_truncated: bool,
}

/// A rendered map and its audit-friendly local counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMap {
    pub text: String,
    pub stats: RepoMapStats,
}

#[derive(Debug, thiserror::Error)]
pub enum RepoMapError {
    #[error("cannot open repository root: {0}")]
    Root(#[source] std::io::Error),
    #[error("repository-map root is not a directory")]
    NotDirectory,
    #[error("repository-map construction was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    relative: String,
    language: &'static str,
    base_score: i64,
}

#[derive(Debug, Clone)]
struct MappedFile {
    relative: String,
    language: &'static str,
    size: u64,
    symbols: Vec<Symbol>,
    score: i64,
    source_truncated: bool,
    symbols_capped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Symbol {
    line: usize,
    kind: &'static str,
    name: String,
}

/// Build a local repository map. `cancelled` is checked before discovery, between walk entries,
/// and between file reads so callers can connect it to their turn cancellation token.
#[allow(clippy::too_many_lines)] // Keeping the bounded walk/read pipeline linear aids auditing.
pub fn build_repo_map(
    root: &Path,
    options: &RepoMapOptions,
    cancelled: impl Fn() -> bool,
) -> Result<RepoMap, RepoMapError> {
    if cancelled() {
        return Err(RepoMapError::Cancelled);
    }
    let root = std::fs::canonicalize(root).map_err(RepoMapError::Root)?;
    if !root.is_dir() {
        return Err(RepoMapError::NotDirectory);
    }
    let limits = options.limits.bounded();
    let excludes = compile_excludes(&options.excluded_globs);
    let query_terms = query_terms(options.query.as_deref());
    let mut stats = RepoMapStats::default();
    let mut candidates = Vec::new();

    let mut builder = WalkBuilder::new(&root);
    builder
        .standard_filters(true)
        .parents(false)
        .git_global(false)
        // `.git/info/exclude` is clone-local state. Letting it influence discovery would make
        // identical committed workspaces produce different maps on different machines.
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .same_file_system(true)
        .max_depth(Some(limits.max_depth))
        .sort_by_file_path(std::cmp::Ord::cmp);

    for result in builder.build() {
        if cancelled() {
            return Err(RepoMapError::Cancelled);
        }
        if stats.walked_entries >= limits.max_walk_entries {
            stats.walk_truncated = true;
            break;
        }
        stats.walked_entries += 1;
        let Ok(entry) = result else {
            stats.skipped_unreadable += 1;
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            stats.skipped_unreadable += 1;
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(relative_path) = entry.path().strip_prefix(&root) else {
            stats.skipped_unsafe += 1;
            continue;
        };
        if excluded(&excludes, relative_path, entry.path()) {
            continue;
        }
        let Some(language) = classify(relative_path) else {
            continue;
        };
        let Some(relative) = portable_relative(relative_path) else {
            stats.skipped_unsafe += 1;
            continue;
        };
        let base_score = candidate_score(&relative, language, &query_terms);
        candidates.push(Candidate {
            path: entry.into_path(),
            relative,
            language,
            base_score,
        });
    }

    stats.candidate_files = candidates.len();
    candidates.sort_by(|left, right| {
        Reverse(left.base_score)
            .cmp(&Reverse(right.base_score))
            .then_with(|| left.relative.cmp(&right.relative))
    });
    if candidates.len() > limits.max_files {
        candidates.truncate(limits.max_files);
        stats.files_truncated = true;
    }

    let mut mapped = Vec::new();
    for candidate in candidates {
        if cancelled() {
            return Err(RepoMapError::Cancelled);
        }
        let remaining = limits.max_total_read_bytes.saturating_sub(stats.bytes_read);
        if remaining == 0 {
            stats.read_truncated = true;
            break;
        }
        let per_file_limit = limits.max_file_bytes.min(remaining);
        match secure_read(&root, &candidate.path, per_file_limit) {
            Ok(read) => {
                stats.bytes_read = stats.bytes_read.saturating_add(read.bytes_read);
                if read.truncated {
                    stats.read_truncated = true;
                }
                let Some(text) = read.text else {
                    stats.skipped_unreadable += 1;
                    continue;
                };
                let symbols =
                    extract_symbols(candidate.language, &text, limits.max_symbols_per_file);
                let score = candidate.base_score
                    + symbol_query_score(&symbols, &query_terms)
                    + i64::try_from(symbols.len()).unwrap_or(i64::MAX).min(32);
                let symbols_capped = symbols.len() == limits.max_symbols_per_file;
                mapped.push(MappedFile {
                    relative: candidate.relative,
                    language: candidate.language,
                    size: read.size,
                    symbols,
                    score,
                    source_truncated: read.truncated,
                    symbols_capped,
                });
            }
            Err(SecureReadError::Unsafe) => stats.skipped_unsafe += 1,
            Err(SecureReadError::Unreadable) => stats.skipped_unreadable += 1,
        }
    }

    mapped.sort_by(|left, right| {
        Reverse(left.score)
            .cmp(&Reverse(right.score))
            .then_with(|| left.relative.cmp(&right.relative))
    });
    stats.mapped_files = mapped.len();
    stats.mapped_symbols = mapped.iter().map(|file| file.symbols.len()).sum();
    let (text, output_truncated) = render(&mapped, stats, limits);
    stats.output_truncated = output_truncated;
    Ok(RepoMap { text, stats })
}

fn compile_excludes(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = GlobBuilder::new(pattern).case_insensitive(true).build() {
            builder.add(glob);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap_or_default())
}

fn excluded(set: &GlobSet, relative: &Path, absolute: &Path) -> bool {
    set.is_match(relative) || set.is_match(absolute)
}

fn portable_relative(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        let component = component.to_string_lossy();
        let mut safe = String::new();
        for character in component.chars() {
            match character {
                '\\' => safe.push_str("\\\\"),
                character if character.is_control() => {
                    safe.extend(character.escape_default());
                }
                character => safe.push(character),
            }
        }
        components.push(safe);
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn query_terms(query: Option<&str>) -> Vec<String> {
    let mut terms: Vec<String> = query
        .unwrap_or_default()
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.len() >= 2)
        .map(str::to_lowercase)
        .collect();
    terms.sort();
    terms.dedup();
    terms.truncate(16);
    terms
}

fn candidate_score(relative: &str, language: &str, terms: &[String]) -> i64 {
    let lower = relative.to_lowercase();
    let mut score = match language {
        "rust" | "python" | "typescript" | "javascript" | "go" | "java" | "kotlin" | "c"
        | "cpp" | "csharp" | "swift" => 100,
        "manifest" => 140,
        "documentation" => 35,
        _ => 70,
    };
    let name = relative.rsplit('/').next().unwrap_or(relative);
    if matches!(
        name,
        "lib.rs"
            | "main.rs"
            | "mod.rs"
            | "main.py"
            | "__init__.py"
            | "index.ts"
            | "index.js"
            | "main.go"
    ) {
        score += 70;
    }
    if matches!(
        name,
        "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml" | "build.gradle"
    ) {
        score += 80;
    }
    let depth = relative.bytes().filter(|byte| *byte == b'/').count();
    score -= i64::try_from(depth).unwrap_or(i64::MAX).min(20) * 2;
    for term in terms {
        if lower == *term {
            score += 2_000;
        } else if lower.contains(term) {
            score += 800;
        }
    }
    score
}

fn symbol_query_score(symbols: &[Symbol], terms: &[String]) -> i64 {
    symbols
        .iter()
        .map(|symbol| symbol.name.to_lowercase())
        .flat_map(|name| {
            terms
                .iter()
                .map(move |term| if name.contains(term) { 500 } else { 0 })
        })
        .sum()
}

fn classify(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if matches!(
        name,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "go.mod"
            | "go.sum"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "Makefile"
            | "Dockerfile"
            | "Justfile"
    ) {
        return Some("manifest");
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "lua" => "lua",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" | "lhs" => "haskell",
        "scala" | "sc" => "scala",
        "proto" => "protobuf",
        "sql" => "sql",
        "vue" => "vue",
        "svelte" => "svelte",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "stylesheet",
        "toml" | "json" | "jsonc" | "yaml" | "yml" | "xml" => "manifest",
        "md" | "mdx" | "rst" | "adoc" => "documentation",
        _ => return None,
    })
}

#[derive(Debug)]
struct SecureRead {
    text: Option<String>,
    size: u64,
    bytes_read: usize,
    truncated: bool,
}

#[derive(Debug)]
enum SecureReadError {
    Unsafe,
    Unreadable,
}

#[cfg(unix)]
fn secure_read(root: &Path, target: &Path, limit: usize) -> Result<SecureRead, SecureReadError> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let relative = target
        .strip_prefix(root)
        .map_err(|_| SecureReadError::Unsafe)?;
    let mut components = relative.components().peekable();
    let root_file = open(
        root,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map_err(|_| SecureReadError::Unsafe)?;
    let mut directory = std::fs::File::from(root_file);
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(SecureReadError::Unsafe);
        };
        let is_last = components.peek().is_none();
        let flags = if is_last {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
        } else {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::DIRECTORY
        };
        let opened =
            openat(&directory, name, flags, Mode::empty()).map_err(|_| SecureReadError::Unsafe)?;
        directory = std::fs::File::from(opened);
    }
    read_open_file(directory, limit)
}

#[cfg(not(unix))]
fn secure_read(root: &Path, target: &Path, limit: usize) -> Result<SecureRead, SecureReadError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| SecureReadError::Unsafe)?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(SecureReadError::Unsafe);
        };
        cursor.push(name);
        if std::fs::symlink_metadata(&cursor)
            .map_err(|_| SecureReadError::Unreadable)?
            .file_type()
            .is_symlink()
        {
            return Err(SecureReadError::Unsafe);
        }
    }
    let canonical = std::fs::canonicalize(&cursor).map_err(|_| SecureReadError::Unreadable)?;
    if !canonical.starts_with(root) {
        return Err(SecureReadError::Unsafe);
    }
    let file = std::fs::File::open(canonical).map_err(|_| SecureReadError::Unreadable)?;
    read_open_file(file, limit)
}

fn read_open_file(mut file: std::fs::File, limit: usize) -> Result<SecureRead, SecureReadError> {
    let metadata = file.metadata().map_err(|_| SecureReadError::Unreadable)?;
    if !metadata.is_file() {
        return Err(SecureReadError::Unsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() > 1 {
            return Err(SecureReadError::Unsafe);
        }
    }
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    (&mut file)
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|_| SecureReadError::Unreadable)?;
    let truncated = metadata.len() > u64::try_from(limit).unwrap_or(u64::MAX);
    let bytes_read = bytes.len();
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) if !text.contains('\0') => Some(text.to_string()),
        Err(error) if truncated && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
            std::str::from_utf8(&bytes)
                .ok()
                .filter(|text| !text.contains('\0'))
                .map(str::to_string)
        }
        Ok(_) | Err(_) => None,
    };
    Ok(SecureRead {
        text,
        size: metadata.len(),
        bytes_read,
        truncated,
    })
}

fn extract_symbols(language: &str, text: &str, limit: usize) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if symbols.len() >= limit {
            break;
        }
        if let Some((kind, name)) = symbol_from_line(language, line) {
            symbols.push(Symbol {
                line: index + 1,
                kind,
                name,
            });
        }
    }
    symbols
}

fn symbol_from_line(language: &str, line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with('*')
    {
        return None;
    }
    match language {
        "rust" => rust_symbol(trimmed),
        "python" => keyword_symbol(trimmed, &["async def", "def", "class"]),
        "typescript" | "javascript" | "vue" | "svelte" => js_symbol(trimmed),
        "go" => go_symbol(trimmed),
        "kotlin" => keyword_symbol(
            strip_prefixes(trimmed, &["public", "private", "internal", "protected"]),
            &[
                "suspend fun",
                "fun",
                "data class",
                "class",
                "interface",
                "object",
                "typealias",
                "enum class",
            ],
        ),
        "swift" => keyword_symbol(
            strip_prefixes(trimmed, &["public", "private", "internal", "open", "final"]),
            &[
                "func",
                "class",
                "struct",
                "enum",
                "protocol",
                "actor",
                "typealias",
            ],
        ),
        "ruby" => keyword_symbol(trimmed, &["def", "class", "module"]),
        "elixir" => keyword_symbol(
            trimmed,
            &[
                "defmodule",
                "defprotocol",
                "defstruct",
                "defmacro",
                "defp",
                "def",
            ],
        ),
        "lua" => lua_symbol(trimmed),
        "shell" => shell_symbol(trimmed),
        "java" | "c" | "cpp" | "csharp" | "scala" | "php" => c_family_symbol(trimmed),
        _ => None,
    }
}

fn rust_symbol(line: &str) -> Option<(&'static str, String)> {
    let line = strip_prefixes(
        line,
        &["pub", "async", "unsafe", "const", "extern", "default"],
    );
    if let Some(rest) = line.strip_prefix("macro_rules!") {
        return clean_name(rest).map(|name| ("macro", name));
    }
    keyword_symbol(
        line,
        &[
            "fn", "struct", "enum", "trait", "union", "type", "mod", "static", "const", "impl",
        ],
    )
}

fn js_symbol(line: &str) -> Option<(&'static str, String)> {
    let line = strip_prefixes(line, &["export", "default", "declare", "async"]);
    if let Some(symbol) = keyword_symbol(
        line,
        &[
            "async function",
            "function",
            "class",
            "interface",
            "type",
            "enum",
            "namespace",
        ],
    ) {
        return Some(symbol);
    }
    for keyword in ["const", "let", "var"] {
        if let Some(rest) = word_prefix(line, keyword)
            && (rest.contains("=>") || rest.contains("function"))
        {
            return clean_name(rest).map(|name| ("function", name));
        }
    }
    None
}

fn go_symbol(line: &str) -> Option<(&'static str, String)> {
    if let Some(mut rest) = word_prefix(line, "func") {
        if rest.starts_with('(') {
            rest = rest.split_once(')')?.1.trim_start();
        }
        return clean_name(rest).map(|name| ("func", name));
    }
    keyword_symbol(line, &["type", "const", "var"])
}

fn lua_symbol(line: &str) -> Option<(&'static str, String)> {
    let line = line.strip_prefix("local ").unwrap_or(line);
    keyword_symbol(line, &["function"])
}

fn shell_symbol(line: &str) -> Option<(&'static str, String)> {
    if let Some(symbol) = keyword_symbol(line, &["function"]) {
        return Some(symbol);
    }
    let open = line.find("()")?;
    let name = line[..open].trim();
    valid_name(name).then(|| ("function", name.to_string()))
}

fn c_family_symbol(line: &str) -> Option<(&'static str, String)> {
    let line = strip_prefixes(
        line,
        &[
            "public",
            "private",
            "protected",
            "internal",
            "static",
            "final",
            "abstract",
            "sealed",
            "open",
            "virtual",
            "override",
            "async",
            "inline",
            "extern",
        ],
    );
    if let Some(symbol) = keyword_symbol(
        line,
        &[
            "class",
            "struct",
            "interface",
            "enum",
            "namespace",
            "trait",
            "record",
            "object",
            "type",
        ],
    ) {
        return Some(symbol);
    }
    let open = line.find('(')?;
    if !line.contains('{') && !line.trim_end().ends_with(';') {
        return None;
    }
    let before = line[..open].trim_end();
    let name = before
        .rsplit(|character: char| {
            character.is_whitespace() || matches!(character, ':' | '*' | '&' | '~')
        })
        .find(|part| !part.is_empty())?;
    if matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "return" | "new"
    ) {
        return None;
    }
    valid_name(name).then(|| ("function", name.to_string()))
}

fn keyword_symbol(line: &str, keywords: &[&'static str]) -> Option<(&'static str, String)> {
    for &keyword in keywords {
        if let Some(rest) = word_prefix(line, keyword)
            && let Some(name) = clean_name(rest)
        {
            let kind = keyword.split_whitespace().last().unwrap_or(keyword);
            return Some((kind, name));
        }
    }
    None
}

fn strip_prefixes<'a>(mut line: &'a str, prefixes: &[&str]) -> &'a str {
    loop {
        let mut stripped = false;
        for prefix in prefixes {
            let rest = word_prefix(line, prefix).or_else(|| {
                line.strip_prefix(prefix)
                    .filter(|rest| rest.starts_with('('))
            });
            if let Some(rest) = rest {
                line = rest.trim_start();
                if line.starts_with('(')
                    && let Some(close) = line.find(')')
                {
                    line = line[close + 1..].trim_start();
                }
                if *prefix == "extern"
                    && line.starts_with('"')
                    && let Some(close) = line[1..].find('"')
                {
                    line = line[close + 2..].trim_start();
                }
                stripped = true;
                break;
            }
        }
        if !stripped {
            return line;
        }
    }
}

fn word_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| rest.trim_start())
}

fn clean_name(input: &str) -> Option<String> {
    let name: String = input
        .trim_start()
        .chars()
        .take_while(|character| {
            character.is_alphanumeric() || matches!(character, '_' | '$' | '.' | ':' | '~')
        })
        .take(128)
        .collect();
    valid_name(&name).then_some(name)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().next().is_some_and(|character| {
            character.is_alphabetic() || matches!(character, '_' | '$' | '~')
        })
}

fn render(files: &[MappedFile], stats: RepoMapStats, limits: RepoMapLimits) -> (String, bool) {
    let mut output = String::new();
    let header = format!(
        "# GrokForge repository map\n{} mapped file(s), {} symbol(s); {} candidate(s), {} walked entry/entries; {} byte(s) read\n",
        stats.mapped_files,
        stats.mapped_symbols,
        stats.candidate_files,
        stats.walked_entries,
        stats.bytes_read
    );
    push_bounded(&mut output, &header, limits.max_output_bytes);
    let mut reached = Vec::new();
    if stats.walk_truncated {
        reached.push("walk-entry cap");
    }
    if stats.files_truncated {
        reached.push("file cap");
    }
    if stats.read_truncated {
        reached.push("read cap or partial file");
    }
    if !reached.is_empty() {
        let bound_notice = format!("bounds reached: {}\n", reached.join(", "));
        push_bounded(&mut output, &bound_notice, limits.max_output_bytes);
    }
    let mut truncated = false;
    for file in files {
        let mut qualifiers = Vec::new();
        if file.source_truncated {
            qualifiers.push("partial read");
        }
        if file.symbols_capped {
            qualifiers.push("symbol cap reached");
        }
        let suffix = if qualifiers.is_empty() {
            String::new()
        } else {
            format!(", {}", qualifiers.join(", "))
        };
        let line = format!(
            "\n{} [{}; {} bytes{}]\n",
            file.relative, file.language, file.size, suffix
        );
        if !push_bounded(&mut output, &line, limits.max_output_bytes) {
            truncated = true;
            break;
        }
        for symbol in &file.symbols {
            let line = format!("  L{} {} {}\n", symbol.line, symbol.kind, symbol.name);
            if !push_bounded(&mut output, &line, limits.max_output_bytes) {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    if truncated {
        let marker = "\n… [repository map truncated at output byte limit]\n";
        if output.len().saturating_add(marker.len()) <= limits.max_output_bytes {
            output.push_str(marker);
        }
    }
    (output, truncated)
}

fn push_bounded(output: &mut String, value: &str, limit: usize) -> bool {
    if output.len().saturating_add(value.len()) > limit {
        return false;
    }
    output.push_str(value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> RepoMapOptions {
        RepoMapOptions {
            limits: RepoMapLimits {
                max_files: 20,
                max_walk_entries: 100,
                max_depth: 8,
                max_file_bytes: 4_096,
                max_total_read_bytes: 32_768,
                max_output_bytes: 8_192,
                max_symbols_per_file: 20,
            },
            ..RepoMapOptions::default()
        }
    }

    #[test]
    fn map_is_deterministic_ignore_aware_and_extracts_symbols() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "pub struct Forge;\npub async fn cast() {}\nimpl Forge {}\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/ignored.rs"), "fn secret() {}\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "src/ignored.rs\n").unwrap();

        let first = build_repo_map(root.path(), &options(), || false).unwrap();
        let second = build_repo_map(root.path(), &options(), || false).unwrap();
        assert_eq!(first, second);
        assert!(first.text.contains("src/lib.rs [rust"));
        assert!(first.text.contains("struct Forge"));
        assert!(first.text.contains("fn cast"));
        assert!(!first.text.contains("ignored.rs"));
        assert!(!first.text.contains("secret"));
    }

    #[test]
    fn clone_local_git_exclude_cannot_hide_source_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git/info")).unwrap();
        std::fs::write(root.path().join(".git/info/exclude"), "clone-local.rs\n").unwrap();
        std::fs::write(
            root.path().join("clone-local.rs"),
            "fn visible_in_every_clone() {}\n",
        )
        .unwrap();

        let map = build_repo_map(root.path(), &options(), || false).unwrap();

        assert!(map.text.contains("clone-local.rs"));
        assert!(map.text.contains("visible_in_every_clone"));
    }

    #[test]
    fn deny_globs_remove_paths_before_they_are_opened() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("safe.rs"), "fn visible() {}\n").unwrap();
        std::fs::write(root.path().join("secret.rs"), "fn password() {}\n").unwrap();
        let mut opts = options();
        opts.excluded_globs.push("**/secret.rs".to_string());
        opts.excluded_globs.push("secret.rs".to_string());
        let map = build_repo_map(root.path(), &opts, || false).unwrap();
        assert!(map.text.contains("visible"));
        assert!(!map.text.contains("secret.rs"));
        assert!(!map.text.contains("password"));
    }

    #[test]
    fn depth_file_read_and_output_limits_are_strict() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("one/two/three")).unwrap();
        for index in 0..5 {
            std::fs::write(
                root.path().join(format!("file{index}.rs")),
                format!("fn symbol_{index}() {{}}\n{}", "x".repeat(200)),
            )
            .unwrap();
        }
        std::fs::write(
            root.path().join("one/two/three/deep.rs"),
            "fn too_deep() {}\n",
        )
        .unwrap();
        let mut opts = options();
        opts.limits.max_files = 2;
        opts.limits.max_depth = 2;
        opts.limits.max_file_bytes = 64;
        opts.limits.max_total_read_bytes = 100;
        opts.limits.max_output_bytes = 256;
        let map = build_repo_map(root.path(), &opts, || false).unwrap();
        assert!(map.stats.files_truncated);
        assert!(map.stats.read_truncated);
        assert!(map.stats.bytes_read <= 100);
        assert!(map.stats.mapped_files <= 2);
        assert!(map.text.len() <= 256);
        assert!(!map.text.contains("too_deep"));
    }

    #[test]
    fn query_ranks_matching_paths_first() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.path().join("billing.rs"), "fn charge() {}\n").unwrap();
        let mut opts = options();
        opts.query = Some("billing charge".to_string());
        let map = build_repo_map(root.path(), &opts, || false).unwrap();
        assert!(map.text.find("billing.rs").unwrap() < map.text.find("alpha.rs").unwrap());
    }

    #[test]
    fn cancellation_is_reported() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            build_repo_map(root.path(), &options(), || true),
            Err(RepoMapError::Cancelled)
        ));
    }

    #[test]
    fn rendered_paths_escape_terminal_controls() {
        let path = Path::new("src/evil\nname.rs");
        assert_eq!(
            portable_relative(path).as_deref(),
            Some("src/evil\\nname.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_hardlinks_are_not_mapped() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "fn outside_secret() {}\n").unwrap();
        symlink(&outside_file, root.path().join("linked.rs")).unwrap();
        std::fs::hard_link(&outside_file, root.path().join("hard.rs")).unwrap();
        let map = build_repo_map(root.path(), &options(), || false).unwrap();
        assert!(!map.text.contains("outside_secret"));
        assert!(!map.text.contains("linked.rs"));
        assert!(!map.text.contains("hard.rs"));
        assert!(map.stats.skipped_unsafe >= 1);
    }
}
