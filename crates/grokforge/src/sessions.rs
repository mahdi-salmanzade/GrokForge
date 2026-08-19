//! Local session lifecycle commands and `grokforge resume`.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use grokforge_core::store::SessionSearchResult;
use grokforge_core::{
    PersistedEffort, RolloutWriter, Session, SessionConfig, SessionMeta, sessions_dir,
};
use grokforge_protocol::{ApprovalPolicy, ImageAttachment, ResponseItem, SandboxMode, SessionId};
use grokforge_xai::{Effort, XaiClient, model_supports_effort};
use serde::Serialize;

/// Human-readable or structured export of the session's currently persisted history.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ExportFormat {
    Markdown,
    Json,
}

/// Print the list of saved sessions.
#[allow(clippy::print_literal)] // static column headers
pub async fn list(query: Option<String>) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    if let Some(query) = query {
        return search_in(&dir, &query).await;
    }
    let metas = SessionMeta::list(&dir).await;
    if metas.is_empty() {
        println!("no saved sessions (looked in {})", dir.display());
        return ExitCode::SUCCESS;
    }
    println!(
        "{:<10}  {:<20}  {:<16}  {}",
        "ID", "MODEL", "WORKSPACE", "TITLE / FIRST PROMPT"
    );
    for m in metas {
        print_session_row(&m, session_label(&m));
    }
    ExitCode::SUCCESS
}

/// Search metadata and complete physical transcripts, including pre-compaction turns.
pub async fn search(query: String) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    search_in(&dir, &query).await
}

async fn search_in(dir: &Path, query: &str) -> ExitCode {
    let results = match SessionMeta::search(dir, query).await {
        Ok(results) => results,
        Err(error) => {
            eprintln!("could not search sessions: {error}");
            return ExitCode::from(2);
        }
    };
    if results.is_empty() {
        println!(
            "no sessions matched `{}`",
            crate::sanitize_terminal_line(query)
        );
        return ExitCode::SUCCESS;
    }
    println!("{} session(s) matched:", results.len());
    for result in results {
        print_search_result(&result);
    }
    ExitCode::SUCCESS
}

fn print_search_result(result: &SessionSearchResult) {
    let detail = if result.snippet.is_empty() {
        session_label(&result.meta)
    } else {
        &result.snippet
    };
    print_session_row(&result.meta, detail);
}

fn print_session_row(meta: &SessionMeta, detail: &str) {
    let short_id: String = meta.session_id.chars().take(8).collect();
    let workspace = meta.workspace.file_name().map_or_else(
        || meta.workspace.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let model = crate::sanitize_terminal_line(&meta.model);
    let workspace = crate::sanitize_terminal_line(&workspace);
    let detail = crate::sanitize_terminal_line(detail);
    println!("{short_id:<10}  {model:<20}  {workspace:<16}  {detail}");
}

fn session_label(meta: &SessionMeta) -> &str {
    meta.title.as_deref().unwrap_or_else(|| {
        if meta.first_prompt.is_empty() {
            "(interactive)"
        } else {
            &meta.first_prompt
        }
    })
}

/// Export one session without contacting xAI or opening its workspace.
pub async fn export(
    id: String,
    format: ExportFormat,
    output: Option<PathBuf>,
    force: bool,
) -> ExitCode {
    let (dir, meta, _) = match target(&id).await {
        Ok(target) => target,
        Err(code) => return code,
    };
    let history = match RolloutWriter::read_all(&meta.rollout(&dir)).await {
        Ok(history) => history,
        Err(error) => {
            eprintln!("could not read session transcript: {error}");
            return ExitCode::from(2);
        }
    };
    let rendered = match format {
        ExportFormat::Markdown => render_markdown(&meta, &history),
        ExportFormat::Json => match serde_json::to_string_pretty(&JsonExport {
            format_version: 1,
            metadata: &meta,
            history: &history,
        }) {
            Ok(json) => format!("{json}\n"),
            Err(error) => {
                eprintln!("could not encode session export: {error}");
                return ExitCode::from(2);
            }
        },
    };

    match output {
        Some(path) => match write_private_export(path.clone(), rendered, force).await {
            Ok(()) => {
                eprintln!(
                    "exported session {} to {}",
                    short_id(&meta.session_id),
                    path.display()
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("could not write session export: {error}");
                ExitCode::from(2)
            }
        },
        None => match write_export_stdout(&rendered) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("could not write session export: {error}");
                ExitCode::from(2)
            }
        },
    }
}

#[derive(Serialize)]
struct JsonExport<'a> {
    format_version: u8,
    metadata: &'a SessionMeta,
    history: &'a [ResponseItem],
}

fn render_markdown(meta: &SessionMeta, history: &[ResponseItem]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "# {}", escape_markdown_inline(session_label(meta)));
    let _ = writeln!(output);
    let _ = writeln!(
        output,
        "- Session: {}",
        markdown_inline_code(&meta.session_id)
    );
    let _ = writeln!(output, "- Model: {}", markdown_inline_code(&meta.model));
    let _ = writeln!(
        output,
        "- Workspace: {}",
        markdown_inline_code(&meta.workspace.display().to_string())
    );
    let _ = writeln!(output, "- Created: `{}`", meta.created_unix);
    if let Some(parent) = &meta.parent_session_id {
        let _ = writeln!(output, "- Forked from: {}", markdown_inline_code(parent));
    }
    let _ = writeln!(output);

    for item in history {
        match item {
            ResponseItem::UserMessage {
                text,
                redactions,
                images,
            } => {
                let _ = writeln!(output, "## User");
                append_redaction_note(&mut output, *redactions);
                append_message(&mut output, text);
                append_image_note(&mut output, images);
            }
            ResponseItem::AssistantMessage { text } => {
                let _ = writeln!(output, "## Grok");
                append_message(&mut output, text);
            }
            ResponseItem::Reasoning { text } => {
                let _ = writeln!(output, "## Reasoning summary");
                append_message(&mut output, text);
            }
            ResponseItem::EncryptedReasoning {
                id,
                status,
                summary,
                ..
            } => {
                let _ = writeln!(output, "## Encrypted reasoning state");
                let _ = writeln!(
                    output,
                    "{} · {}",
                    markdown_inline_code(id),
                    markdown_inline_code(status)
                );
                let summary =
                    serde_json::to_string_pretty(summary).unwrap_or_else(|_| "[]".to_string());
                append_code_block(&mut output, "json", &summary);
                let _ = writeln!(
                    output,
                    "_Encrypted provider state omitted from Markdown export._\n"
                );
            }
            ResponseItem::ProviderOutput { item } => {
                let _ = writeln!(output, "## Provider output");
                let encoded =
                    serde_json::to_string_pretty(item).unwrap_or_else(|_| "null".to_string());
                append_code_block(&mut output, "json", &encoded);
            }
            ResponseItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                let _ = writeln!(output, "## Tool call · {}", escape_markdown_inline(name));
                let _ = writeln!(output, "Call: {}\n", markdown_inline_code(id.as_str()));
                append_code_block(&mut output, "json", arguments);
            }
            ResponseItem::ToolResult {
                id,
                content,
                is_error,
                redactions,
            } => {
                let status = if *is_error { "error" } else { "result" };
                let _ = writeln!(output, "## Tool {status}");
                let _ = writeln!(output, "Call: {}\n", markdown_inline_code(id.as_str()));
                append_redaction_note(&mut output, *redactions);
                append_code_block(&mut output, "text", content);
            }
            ResponseItem::CompactionSummary { text, redactions } => {
                let _ = writeln!(output, "## Compaction summary");
                append_redaction_note(&mut output, *redactions);
                append_message(&mut output, text);
            }
            ResponseItem::CompactionCheckpoint { .. } => {
                let _ = writeln!(output, "_Internal compaction checkpoint omitted._\n");
            }
        }
    }
    output
}

fn append_message(output: &mut String, text: &str) {
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');
}

fn append_redaction_note(output: &mut String, redactions: usize) {
    use std::fmt::Write as _;

    if redactions > 0 {
        let _ = writeln!(output, "_{redactions} secret(s) redacted._\n");
    }
}

fn append_image_note(output: &mut String, images: &[ImageAttachment]) {
    use std::fmt::Write as _;

    if images.is_empty() {
        return;
    }
    let types = images
        .iter()
        .map(|image| escape_markdown_inline(&image.mime_type))
        .collect::<Vec<_>>()
        .join(", ");
    let count = images.len();
    let _ = writeln!(
        output,
        "_{count} image attachment(s): {types}. Binary data is preserved in JSON exports._\n"
    );
}

fn append_code_block(output: &mut String, language: &str, content: &str) {
    use std::fmt::Write as _;

    let fence_length = longest_backtick_run(content).saturating_add(1).max(3);
    let fence = "`".repeat(fence_length);
    let _ = writeln!(output, "{fence}{language}");
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    let _ = writeln!(output, "{fence}\n");
}

fn longest_backtick_run(value: &str) -> usize {
    value
        .chars()
        .fold((0usize, 0usize), |(longest, current), character| {
            if character == '`' {
                (
                    longest.max(current.saturating_add(1)),
                    current.saturating_add(1),
                )
            } else {
                (longest, 0)
            }
        })
        .0
}

fn escape_markdown_inline(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if matches!(character, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>') {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect()
}

fn markdown_inline_code(value: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(value).saturating_add(1).max(1));
    let padding = if value.starts_with('`')
        || value.starts_with(' ')
        || value.ends_with('`')
        || value.ends_with(' ')
    {
        " "
    } else {
        ""
    };
    format!("{fence}{padding}{value}{padding}{fence}")
}

fn write_export_stdout(rendered: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let terminal = stdout.is_terminal();
    let mut stdout = stdout.lock();
    if terminal {
        stdout.write_all(crate::sanitize_terminal(rendered).as_bytes())
    } else {
        stdout.write_all(rendered.as_bytes())
    }
}

async fn write_private_export(path: PathBuf, rendered: String, force: bool) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || write_private_export_blocking(&path, &rendered, force))
        .await
        .map_err(|error| std::io::Error::other(format!("export writer task failed: {error}")))?
}

fn write_private_export_blocking(path: &Path, rendered: &str, force: bool) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to overwrite a symlink",
        ));
    }

    let file = if force && path.exists() {
        let file = options.open(path)?;
        validate_export_target(&file)?;
        file.set_len(0)?;
        file
    } else {
        options.create_new(true).open(path)?
    };
    validate_export_target(&file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut file = file;
    file.write_all(rendered.as_bytes())?;
    file.sync_all()
}

fn validate_export_target(file: &std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session export destination is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to overwrite a file with multiple hard links",
            ));
        }
    }
    Ok(())
}

/// Create a new local session containing a point-in-time copy of the selected history.
pub async fn fork(id: String, title: Option<String>) -> ExitCode {
    let (dir, meta, _) = match target(&id).await {
        Ok(target) => target,
        Err(code) => return code,
    };
    match SessionMeta::fork(&dir, &meta, title.as_deref()).await {
        Ok(forked) => {
            println!(
                "forked {} -> {}",
                short_id(&meta.session_id),
                forked.session_id
            );
            println!(
                "resume with: grokforge resume {}",
                short_id(&forked.session_id)
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("could not fork session: {error}");
            ExitCode::from(2)
        }
    }
}

/// Assign a human-readable title to a saved session.
pub async fn rename(id: String, title: String) -> ExitCode {
    let (dir, _, session_id) = match target(&id).await {
        Ok(target) => target,
        Err(code) => return code,
    };
    match SessionMeta::rename(&dir, session_id, &title).await {
        Ok(meta) => {
            println!(
                "renamed {} to `{}`",
                short_id(&meta.session_id),
                crate::sanitize_terminal_line(session_label(&meta))
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("could not rename session: {error}");
            ExitCode::from(2)
        }
    }
}

/// Delete one saved session after an explicit interactive confirmation or `--force`.
pub async fn delete(id: String, force: bool) -> ExitCode {
    if id.trim().len() < 8 {
        eprintln!(
            "delete requires at least the 8-character session id shown by `grokforge sessions`"
        );
        return ExitCode::from(2);
    }
    let (dir, meta, session_id) = match target(&id).await {
        Ok(target) => target,
        Err(code) => return code,
    };
    if !force {
        match confirm_delete(&meta) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("deletion cancelled");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("{error}; pass --force for a non-interactive deletion");
                return ExitCode::from(2);
            }
        }
    }
    match SessionMeta::delete(&dir, session_id).await {
        Ok(()) => {
            println!("deleted session {}", short_id(&meta.session_id));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("could not delete session: {error}");
            ExitCode::from(2)
        }
    }
}

fn confirm_delete(meta: &SessionMeta) -> std::io::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "interactive confirmation requires a terminal",
        ));
    }
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "Delete session {} (`{}`) from {}?",
        short_id(&meta.session_id),
        crate::sanitize_terminal_line(session_label(meta)),
        crate::sanitize_terminal_line(&meta.workspace.display().to_string())
    )?;
    write!(stderr, "Type the full session id to confirm: ")?;
    stderr.flush()?;
    let mut response = String::new();
    std::io::stdin().read_line(&mut response)?;
    Ok(response.trim() == meta.session_id)
}

async fn target(id: &str) -> Result<(PathBuf, SessionMeta, SessionId), ExitCode> {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return Err(ExitCode::from(2));
        }
    };
    let metas = SessionMeta::list(&dir).await;
    let meta = match pick(&metas, Some(id)) {
        Ok(meta) => meta.clone(),
        Err(error) => {
            print_pick_error(error);
            return Err(ExitCode::from(2));
        }
    };
    let session_id = match SessionId::parse_str(&meta.session_id) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("invalid persisted session id: {error}");
            return Err(ExitCode::from(2));
        }
    };
    Ok((dir, meta, session_id))
}

fn print_pick_error(error: PickError) {
    match error {
        PickError::NoMatch => eprintln!("no matching session found (see `grokforge sessions`)"),
        PickError::EmptyPrefix => eprintln!("session id prefix must not be empty"),
        PickError::Ambiguous => {
            eprintln!("session id prefix is ambiguous; provide more characters");
        }
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Resume a session: load its transcript and reopen the TUI continuing from it.
#[allow(clippy::too_many_lines)]
pub async fn resume(
    id: Option<String>,
    trust_project_mcp: bool,
    trust_project_config: bool,
    trust_project_tools: bool,
    model_override: Option<String>,
    effort_override: Option<String>,
) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    let mut metas = SessionMeta::list(&dir).await;
    if id.is_none() {
        let current = match std::env::current_dir().and_then(std::fs::canonicalize) {
            Ok(workspace) => workspace,
            Err(error) => {
                eprintln!("could not determine current workspace: {error}");
                return ExitCode::from(2);
            }
        };
        let workspace = project_root(&current).unwrap_or(current);
        metas.retain(|meta| project_root(&meta.workspace).is_some_and(|path| path == workspace));
    }
    let meta = match pick(&metas, id.as_deref()) {
        Ok(meta) => meta.clone(),
        Err(PickError::NoMatch) => {
            eprintln!("no matching session found (see `grokforge sessions`)");
            return ExitCode::from(2);
        }
        Err(PickError::EmptyPrefix) => {
            eprintln!("session id prefix must not be empty");
            return ExitCode::from(2);
        }
        Err(PickError::Ambiguous) => {
            eprintln!("session id prefix is ambiguous; provide more characters");
            return ExitCode::from(2);
        }
    };

    let session_id = match SessionId::parse_str(&meta.session_id) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("invalid persisted session id: {error}");
            return ExitCode::from(2);
        }
    };
    // Take the lifetime lock and atomically repair/read the rollout before any workspace Git
    // inspection, model validation, or external-process setup.
    let (rollout, items) = match RolloutWriter::open_and_read(&dir, session_id).await {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("could not exclusively resume session: {error}");
            return ExitCode::from(2);
        }
    };

    let workspace = match std::fs::canonicalize(&meta.workspace) {
        Ok(workspace) if workspace.is_dir() => workspace,
        Ok(_) => {
            eprintln!("saved workspace is not a directory");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("could not resolve saved workspace: {error}");
            return ExitCode::from(2);
        }
    };
    let current_identity = grokforge_git::Git::discover(&workspace)
        .and_then(|git| std::fs::canonicalize(git.root()).ok())
        .unwrap_or_else(|| workspace.clone());
    if let Some(expected) = &meta.workspace_identity {
        // `expected` was canonicalized when metadata was written. Do not canonicalize it again:
        // doing so would let a later symlink replacement rewrite the expected identity too.
        if expected != &current_identity {
            eprintln!("saved workspace no longer matches the recorded project identity");
            return ExitCode::from(2);
        }
    }
    if !meta.fingerprint_matches(&workspace) {
        eprintln!("saved workspace was replaced or no longer matches its Git metadata");
        return ExitCode::from(2);
    }
    let settings = match grokforge_config::Config::load_with_project_config(
        &workspace,
        trust_project_config,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "configuration error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let configured_effort = settings.agent.effort.map(configured_effort);
    let resume_default = meta
        .effort
        .map_or(configured_effort, PersistedEffort::effective);
    let persist_effort_override = effort_override.is_some();
    let Ok(effort) = resolve_effort(effort_override.as_deref(), resume_default) else {
        eprintln!("invalid --effort value (auto|low|medium|high|xhigh)");
        return ExitCode::from(2);
    };
    let model = model_override.unwrap_or_else(|| meta.model.clone());

    let Some(api_key) = crate::credentials::resolve(false).await else {
        return ExitCode::from(3);
    };
    let base_url =
        std::env::var("XAI_BASE_URL").unwrap_or_else(|_| settings.provider.grok.base_url.clone());
    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return ExitCode::from(2);
        }
    };
    let model_catalog = match crate::model_catalog_startup(&client, &model).await {
        Ok(models) => models,
        Err(code) => return code,
    };
    let selected_model = model_catalog.iter().find(|candidate| {
        candidate.id == model || candidate.aliases.iter().any(|alias| alias == &model)
    });
    let active_model = selected_model.map_or(model, |candidate| candidate.id.clone());
    let context_window_tokens = selected_model.and_then(|candidate| candidate.context_window);
    if effort.is_some_and(|effort| !model_supports_effort(&active_model, effort)) {
        eprintln!("reasoning effort `xhigh` requires an xAI multi-agent model");
        return ExitCode::from(2);
    }

    let mut config = SessionConfig::new(workspace, active_model.clone())
        .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);
    config.plan_model = settings.agent.plan_model.clone();
    config.model_catalog = model_catalog;
    config.context_window_tokens = context_window_tokens;
    config.max_iterations = settings.agent.max_iterations;
    config.auto_compact = settings.agent.auto_compact;
    config.compaction_trigger_bytes = settings.agent.compaction_trigger_bytes;
    config.compaction_keep_tail = settings.agent.compaction_keep_tail;
    config.effort = effort;
    let session = match Session::with_id_and_history(config, &meta.session_id, items) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("invalid persisted session id: {error}");
            return ExitCode::from(2);
        }
    };
    let model_changed = active_model != meta.model;
    let metadata_update = match (model_changed, persist_effort_override) {
        (true, true) => {
            SessionMeta::update_model_and_effort(&dir, session_id, active_model, effort).await
        }
        (true, false) => SessionMeta::update_model(&dir, session_id, active_model).await,
        (false, true) => SessionMeta::update_effort(&dir, session_id, effort).await,
        (false, false) => Ok(()),
    };
    if let Err(error) = metadata_update {
        eprintln!("could not persist resumed runtime overrides: {error}");
        return ExitCode::from(2);
    }

    eprintln!(
        "resuming session {} ({} items)",
        &meta.session_id[..8.min(meta.session_id.len())],
        session.history.len()
    );
    let mcp_oauth_tokens = if trust_project_mcp {
        crate::credentials::mcp_access_tokens(&session.config.workspace_root).await
    } else {
        std::collections::BTreeMap::new()
    };
    match grokforge_tui::run_locked_session_with_mcp_oauth(
        client,
        session,
        rollout,
        "auto".to_string(),
        trust_project_mcp,
        trust_project_tools,
        mcp_oauth_tokens,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::from(1)
        }
    }
}

fn resolve_effort(
    override_value: Option<&str>,
    configured: Option<Effort>,
) -> Result<Option<Effort>, ()> {
    match override_value {
        Some("auto") => Ok(None),
        Some("low") => Ok(Some(Effort::Low)),
        Some("medium") => Ok(Some(Effort::Medium)),
        Some("high") => Ok(Some(Effort::High)),
        Some("xhigh") => Ok(Some(Effort::Xhigh)),
        Some(_) => Err(()),
        None => Ok(configured),
    }
}

fn configured_effort(effort: grokforge_config::Effort) -> Effort {
    match effort {
        grokforge_config::Effort::Low => Effort::Low,
        grokforge_config::Effort::Medium => Effort::Medium,
        grokforge_config::Effort::High => Effort::High,
        grokforge_config::Effort::Xhigh => Effort::Xhigh,
    }
}

/// Choose a session by id prefix, or the most recent one.
fn pick<'a>(metas: &'a [SessionMeta], id: Option<&str>) -> Result<&'a SessionMeta, PickError> {
    match id {
        Some(prefix) if prefix.trim().is_empty() => Err(PickError::EmptyPrefix),
        Some(prefix) => {
            let mut matches = metas
                .iter()
                .filter(|meta| meta.session_id.starts_with(prefix));
            let first = matches.next().ok_or(PickError::NoMatch)?;
            if matches.next().is_some() {
                Err(PickError::Ambiguous)
            } else {
                Ok(first)
            }
        }
        None => metas.first().ok_or(PickError::NoMatch),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickError {
    NoMatch,
    EmptyPrefix,
    Ambiguous,
}

fn project_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    Some(grokforge_git::Git::discover(&canonical).map_or(canonical, |git| git.root().to_path_buf()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            session_id: id.to_string(),
            title: None,
            parent_session_id: None,
            workspace: PathBuf::from("/workspace"),
            workspace_identity: None,
            workspace_fingerprint: None,
            model: "model".to_string(),
            effort: None,
            created_unix: 0,
            created_unix_nanos: 0,
            first_prompt: String::new(),
        }
    }

    #[test]
    fn pick_rejects_empty_and_ambiguous_prefixes() {
        let metas = vec![meta("abcd-1"), meta("abcd-2")];
        assert!(matches!(
            pick(&metas, Some("")),
            Err(PickError::EmptyPrefix)
        ));
        assert!(matches!(
            pick(&metas, Some("abcd")),
            Err(PickError::Ambiguous)
        ));
        assert!(matches!(
            pick(&metas, Some("missing")),
            Err(PickError::NoMatch)
        ));
        assert_eq!(
            pick(&metas, Some("abcd-1")).map(|m| m.session_id.as_str()),
            Ok("abcd-1")
        );
    }

    #[test]
    fn markdown_export_is_readable_and_uses_collision_safe_fences() {
        let mut metadata = meta("00000000-0000-0000-0000-000000000000");
        metadata.title = Some("Named *session*".into());
        let history = vec![
            ResponseItem::user("Please inspect this"),
            ResponseItem::ToolResult {
                id: grokforge_protocol::ToolCallId::from_raw("call-1"),
                content: "value\n```\nmore".into(),
                is_error: false,
                redactions: 1,
            },
            ResponseItem::EncryptedReasoning {
                id: "reasoning-1".into(),
                status: "completed".into(),
                summary: vec![serde_json::json!({"text":"safe summary"})],
                encrypted_content: "ciphertext-must-not-appear".into(),
            },
        ];
        let markdown = render_markdown(&metadata, &history);
        assert!(markdown.starts_with("# Named \\*session\\*"));
        assert!(markdown.contains("````text\nvalue\n```\nmore\n````"));
        assert!(markdown.contains("1 secret(s) redacted"));
        assert!(markdown.contains("safe summary"));
        assert!(!markdown.contains("ciphertext-must-not-appear"));
    }

    #[test]
    fn private_export_requires_force_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.md");
        write_private_export_blocking(&path, "first", false).unwrap();
        let error = write_private_export_blocking(&path, "second", false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        write_private_export_blocking(&path, "second", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_export_refuses_symlinks_and_hard_links() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        let symlink_path = dir.path().join("symlink");
        symlink(&target, &symlink_path).unwrap();
        assert!(write_private_export_blocking(&symlink_path, "replace", true).is_err());

        let hard_link = dir.path().join("hard-link");
        std::fs::hard_link(&target, &hard_link).unwrap();
        assert!(write_private_export_blocking(&hard_link, "replace", true).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep");
    }

    #[test]
    fn resume_effort_override_wins_over_configured_default() {
        assert_eq!(
            resolve_effort(Some("high"), Some(Effort::Low)),
            Ok(Some(Effort::High))
        );
        assert_eq!(
            resolve_effort(None, Some(Effort::Xhigh)),
            Ok(Some(Effort::Xhigh))
        );
        assert_eq!(resolve_effort(Some("auto"), Some(Effort::High)), Ok(None));
        assert_eq!(resolve_effort(Some("extreme"), None), Err(()));
    }

    #[test]
    fn persisted_effort_beats_config_while_legacy_metadata_falls_back() {
        let configured = Some(Effort::Low);
        assert_eq!(PersistedEffort::High.effective(), Some(Effort::High));
        assert_eq!(PersistedEffort::Auto.effective(), None);
        assert_eq!(
            None::<PersistedEffort>.map_or(configured, PersistedEffort::effective),
            Some(Effort::Low)
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_root_groups_subdirectories_but_not_other_repositories() {
        use std::process::Command;

        let repo = tempfile::tempdir().expect("repo");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .expect("git init")
                .success()
        );
        let nested = repo.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let other = tempfile::tempdir().expect("other");
        assert_eq!(project_root(&nested), project_root(repo.path()));
        assert_ne!(project_root(&nested), project_root(other.path()));
    }
}
