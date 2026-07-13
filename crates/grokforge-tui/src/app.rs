//! The interactive TUI application: an async event loop over terminal input, agent events, and
//! approval requests, rendering a scrolling transcript, a composer, a status line, and an
//! approval modal.
//!
//! This is the first working cut. It uses the alternate screen for robustness; the inline
//! viewport + native-scrollback render pipeline (the inline-scrollback differentiator in the design
//! docs) is the planned upgrade and slots in behind the same event/op flow.

use std::any::Any;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{FutureExt, StreamExt};
use grokforge_core::commands::{self, CommandDoc};
use grokforge_core::skills::{self, SkillDoc};
use grokforge_core::{Agent, RolloutWriter, Session, TurnCancellation};
use grokforge_protocol::{Decision, DenialClass, EventMsg, ResponseItem, Usage};
use grokforge_xai::ServerTool;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

use crate::approver::PendingApproval;

const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 2_048;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 48 * 1024;
const MAX_RENDER_TRANSCRIPT_ENTRIES: usize = 512;
const MAX_RENDER_TRANSCRIPT_BYTES: usize = 48 * 1024;
const APPROVAL_PAGE_BYTES: usize = 8 * 1024;
const REDRAW_INTERVAL: Duration = Duration::from_millis(33);

// GrokForge's UI palette. Explicit colors make the product identity consistent across terminal
// themes; every foreground/background pair is intentionally high-contrast. The interface still
// makes sense without color because state and ownership always have a text label or glyph too.
const CANVAS: Color = Color::Rgb(8, 10, 15);
const SURFACE: Color = Color::Rgb(16, 19, 26);
const SURFACE_RAISED: Color = Color::Rgb(23, 27, 36);
// Borders are UI structure, not decoration: this clears the 3:1 non-text contrast threshold
// against `SURFACE` while remaining quieter than body text.
const BORDER: Color = Color::Rgb(96, 104, 126);
const TEXT: Color = Color::Rgb(232, 235, 241);
const MUTED: Color = Color::Rgb(137, 144, 160);
const FAINT: Color = Color::Rgb(124, 131, 149);
const ACCENT: Color = Color::Rgb(255, 90, 31);
const ACCENT_SOFT: Color = Color::Rgb(255, 139, 92);
const USER: Color = Color::Rgb(79, 199, 255);
const SUCCESS: Color = Color::Rgb(73, 211, 142);
const WARNING: Color = Color::Rgb(246, 197, 93);
const DANGER: Color = Color::Rgb(255, 105, 120);
const TOOL: Color = Color::Rgb(190, 148, 255);
const GIT: Color = Color::Rgb(96, 165, 250);

/// Presentation fallbacks for terminals that cannot reliably display the default palette or
/// width-critical UI glyphs. `NO_COLOR` follows the ecosystem convention; `TERM=dumb` enables
/// both fallbacks, and `GROKFORGE_ASCII` can request just the ASCII treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayMode {
    color: bool,
    ascii: bool,
}

impl DisplayMode {
    fn from_environment() -> Self {
        let dumb = std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        Self {
            color: !dumb && !environment_flag("NO_COLOR"),
            ascii: dumb || environment_flag("GROKFORGE_ASCII"),
        }
    }
}

fn environment_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// A finalized transcript entry.
#[derive(Debug)]
enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        text: String,
        sandboxed: Option<bool>,
    },
    ToolResult {
        ok: bool,
        text: String,
        denial: Option<DenialClass>,
    },
    Approval {
        summary: String,
        decision: String,
        approved: bool,
        auto: bool,
    },
    Retry {
        attempt: u32,
        reason: String,
    },
    Skill {
        name: String,
        description: String,
        path: String,
    },
    ServerTool {
        name: String,
        enabled: bool,
    },
    Git(String),
    Error(String),
    Info(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownState {
    Active,
    Requested,
    Ready,
}

/// Lazily paginates an approval across the actual modal viewport. Page boundaries are cached,
/// so ordinary redraws inspect at most [`APPROVAL_PAGE_BYTES`] instead of rescanning a large
/// command. The byte cap also bounds pathological zero-width Unicode input.
#[derive(Debug)]
struct ApprovalDetail {
    text: String,
    pager: RefCell<ApprovalPager>,
}

#[derive(Debug)]
struct ApprovalPager {
    width: usize,
    rows: usize,
    page_starts: Vec<usize>,
    current: usize,
    complete: bool,
}

#[derive(Debug)]
struct ApprovalPage {
    body: String,
    number: usize,
    total: Option<usize>,
    start: usize,
    end: usize,
    bytes: usize,
}

impl ApprovalDetail {
    fn new(text: String) -> Self {
        Self {
            text,
            // These defaults only matter if a key arrives before the first modal render. The
            // first render reflows around the actual viewport while preserving the byte anchor.
            pager: RefCell::new(ApprovalPager {
                width: 80,
                rows: 8,
                page_starts: vec![0],
                current: 0,
                complete: false,
            }),
        }
    }

    fn previous(&self, count: usize) {
        let mut pager = self.pager.borrow_mut();
        pager.current = pager.current.saturating_sub(count);
    }

    fn next(&self, count: usize) {
        let mut pager = self.pager.borrow_mut();
        for _ in 0..count {
            if pager.current + 1 < pager.page_starts.len() {
                pager.current += 1;
            } else if !self.extend_pages(&mut pager) {
                break;
            } else {
                pager.current += 1;
            }
        }
    }

    fn home(&self) {
        self.pager.borrow_mut().current = 0;
    }

    fn end(&self) {
        let mut pager = self.pager.borrow_mut();
        while self.extend_pages(&mut pager) {}
        pager.current = pager.page_starts.len().saturating_sub(1);
    }

    fn page(&self, width: u16, rows: u16) -> ApprovalPage {
        let width = usize::from(width.max(1));
        let rows = usize::from(rows.max(1));
        let mut pager = self.pager.borrow_mut();
        self.reflow_if_needed(&mut pager, width, rows);

        let page = pager.current.min(pager.page_starts.len().saturating_sub(1));
        pager.current = page;
        let start = pager.page_starts[page];
        let end = pager
            .page_starts
            .get(page + 1)
            .copied()
            .unwrap_or_else(|| approval_page_end(&self.text, start, pager.width, pager.rows));
        if end >= self.text.len() {
            pager.complete = true;
        }
        let total = pager.complete.then_some(pager.page_starts.len());
        ApprovalPage {
            body: approval_page_body(&self.text[start..end], pager.width),
            number: page + 1,
            total,
            start,
            end,
            bytes: self.text.len(),
        }
    }

    fn reflow_if_needed(&self, pager: &mut ApprovalPager, width: usize, rows: usize) {
        if pager.width == width && pager.rows == rows {
            return;
        }

        let anchor = pager.page_starts.get(pager.current).copied().unwrap_or(0);
        pager.width = width;
        pager.rows = rows;
        pager.page_starts.clear();
        pager.page_starts.push(0);
        pager.current = 0;
        pager.complete = self.text.is_empty();

        // A resize is the only operation that may rescan the prefix. Ordinary redraw and page
        // navigation remain bounded, and the currently viewed byte stays visible after reflow.
        while !pager.complete {
            let start = *pager.page_starts.last().unwrap_or(&0);
            let end = approval_page_end(&self.text, start, width, rows);
            if end > anchor || end >= self.text.len() {
                if end >= self.text.len() {
                    pager.complete = true;
                }
                break;
            }
            pager.page_starts.push(end);
            pager.current += 1;
        }
    }

    /// Extends the current layout by one page. Returns true only when a new page exists.
    fn extend_pages(&self, pager: &mut ApprovalPager) -> bool {
        if pager.complete {
            return false;
        }
        let start = *pager.page_starts.last().unwrap_or(&0);
        let end = approval_page_end(&self.text, start, pager.width, pager.rows);
        if end >= self.text.len() {
            pager.complete = true;
            return false;
        }
        pager.page_starts.push(end);
        true
    }
}

/// The running application state.
pub struct App {
    agent: Arc<Agent>,
    session: Option<Session>,
    rollout: Option<RolloutWriter>,
    transcript: Vec<Entry>,
    transcript_bytes: usize,
    omitted_entries: usize,
    streaming: Option<String>,
    reasoning: Option<String>,
    composer: String,
    scroll: u16,
    follow: bool,
    pending: Option<PendingApproval>,
    /// Sanitized and cached once when the request arrives. Full approval details remain visible;
    /// redraws do not repeatedly format or scan a potentially large command.
    approval_detail: Option<ApprovalDetail>,
    running: bool,
    shutdown: ShutdownState,
    status_model: String,
    status_preset: String,
    active_tool: Option<String>,
    stream_retry: Option<u32>,
    usage: Usage,
    ledger_bytes: usize,
    ledger_sources: usize,
    ledger_redactions: usize,
    available_skills: Vec<SkillDoc>,
    project_commands: Vec<CommandDoc>,
    display_mode: DisplayMode,

    // The channel stays open for the app's lifetime because `agent` (Arc) holds the sender.
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
    approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
    turn_handle: Option<JoinHandle<TurnOutcome>>,
    turn_cancellation: Option<TurnCancellation>,
    turn_complete_seen: bool,
    undo_handle: Option<JoinHandle<Entry>>,
}

#[derive(Debug)]
struct TurnOutcome {
    session: Session,
    rollout: Option<RolloutWriter>,
    panic: Option<String>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("transcript_len", &self.transcript.len())
            .field("running", &self.running)
            .field("has_pending_approval", &self.pending.is_some())
            .field("has_pending_undo", &self.undo_handle.is_some())
            .finish_non_exhaustive()
    }
}

impl App {
    #[must_use]
    pub fn new(
        agent: Arc<Agent>,
        session: Session,
        rollout: Option<RolloutWriter>,
        events_rx: mpsc::UnboundedReceiver<EventMsg>,
        approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
        status_model: String,
        status_preset: String,
    ) -> Self {
        let resumed = session.history.len();
        let available_skills = skills::discover(&session.config.workspace_root);
        let project_commands = commands::discover(&session.config.workspace_root);
        let mut transcript = Vec::new();
        let mut omitted_entries = 0;
        if resumed > 0 {
            transcript.push(Entry::Info(format!(
                "resumed session with {resumed} prior transcript item(s)"
            )));
            let (tail, omitted) = resumed_transcript_tail(&session.history);
            omitted_entries = omitted;
            transcript.extend(tail);
        }
        let transcript_bytes = transcript.iter().map(entry_bytes).sum();
        Self {
            agent,
            session: Some(session),
            rollout,
            transcript,
            transcript_bytes,
            omitted_entries,
            streaming: None,
            reasoning: None,
            composer: String::new(),
            scroll: 0,
            follow: true,
            pending: None,
            approval_detail: None,
            running: false,
            shutdown: ShutdownState::Active,
            status_model,
            status_preset,
            active_tool: None,
            stream_retry: None,
            usage: Usage::default(),
            ledger_bytes: 0,
            ledger_sources: 0,
            ledger_redactions: 0,
            available_skills,
            project_commands,
            display_mode: DisplayMode::from_environment(),
            events_rx,
            approvals_rx,
            turn_handle: None,
            turn_cancellation: None,
            turn_complete_seen: false,
            undo_handle: None,
        }
    }

    pub(crate) fn push_info(&mut self, message: impl Into<String>) {
        self.push_entry(Entry::Info(message.into()));
    }

    fn push_entry(&mut self, entry: Entry) {
        let entry = bounded_entry(entry);
        self.transcript_bytes = self.transcript_bytes.saturating_add(entry_bytes(&entry));
        self.transcript.push(entry);
        while self.transcript.len() > MAX_TRANSCRIPT_ENTRIES
            || self.transcript_bytes > MAX_TRANSCRIPT_BYTES
        {
            if self.transcript.is_empty() {
                break;
            }
            let removed = self.transcript.remove(0);
            self.transcript_bytes = self.transcript_bytes.saturating_sub(entry_bytes(&removed));
            self.omitted_entries = self.omitted_entries.saturating_add(1);
        }
    }

    fn request_quit(&mut self) {
        self.shutdown = ShutdownState::Requested;
        if let Some(cancellation) = &self.turn_cancellation {
            cancellation.cancel();
        }
        if let Some(pending) = self.pending.take() {
            let _ = pending.respond.send(Decision::Abort);
        }
        self.approval_detail = None;
        self.finish_quit_if_quiescent();
    }

    fn finish_quit_if_quiescent(&mut self) {
        if self.shutdown == ShutdownState::Requested
            && self.turn_handle.is_none()
            && self.undo_handle.is_none()
            && !self.running
        {
            self.shutdown = ShutdownState::Ready;
        }
    }

    async fn await_quiescence(&mut self) {
        if let Some(cancellation) = &self.turn_cancellation {
            cancellation.cancel();
        }
        if let Some(handle) = self.turn_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.undo_handle.take() {
            let _ = handle.await;
        }
        self.running = false;
    }

    /// Run the event loop until the user quits.
    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        let mut input = EventStream::new();
        let mut redraw = tokio::time::interval(REDRAW_INTERVAL);
        redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut redraw_needed = true;
        while self.shutdown != ShutdownState::Ready {
            tokio::select! {
                _ = redraw.tick() => {
                    if redraw_needed {
                        if let Err(error) = terminal.draw(|f| self.render(f)) {
                            self.request_quit();
                            self.await_quiescence().await;
                            return Err(error);
                        }
                        redraw_needed = false;
                    }
                }
                maybe_event = input.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) => self.on_terminal_key(key),
                        Some(Ok(Event::Paste(text))) => self.on_paste(&text),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            self.push_entry(Entry::Error(format!("terminal input failed: {error}")));
                            self.request_quit();
                        }
                        None => {
                            self.push_entry(Entry::Error("terminal input stream closed".to_string()));
                            self.request_quit();
                        }
                    }
                    redraw_needed = true;
                }
                Some(msg) = self.events_rx.recv() => {
                    self.on_agent_event(msg);
                    redraw_needed = true;
                }
                Some(pending) = self.approvals_rx.recv() => {
                    self.on_approval_request(pending);
                    redraw_needed = true;
                }
                result = wait_for_turn(&mut self.turn_handle), if self.turn_handle.is_some() => {
                    self.finish_turn(result);
                    redraw_needed = true;
                }
                result = wait_for_undo(&mut self.undo_handle), if self.undo_handle.is_some() => {
                    self.finish_undo(result);
                    redraw_needed = true;
                }
            }
        }
        self.await_quiescence().await;
        Ok(())
    }

    fn on_terminal_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Release {
            self.on_key(key);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }

        // Approval modal captures input.
        if self.pending.is_some() {
            self.on_approval_key(key);
            return;
        }

        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
            {
                if self.composer.len().saturating_add(c.len_utf8()) <= MAX_COMPOSER_BYTES {
                    self.composer.push(c);
                }
            }
            KeyCode::Backspace => {
                self.composer.pop();
            }
            KeyCode::Up => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(1);
                if self.scroll == 0 {
                    self.follow = true;
                }
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.follow = true;
                }
            }
            KeyCode::End => {
                self.follow = true;
                self.scroll = 0;
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, value: &str) {
        if self.pending.is_some() || self.running {
            return;
        }
        let value = safe_terminal_text(value);
        let remaining = MAX_COMPOSER_BYTES.saturating_sub(self.composer.len());
        let mut used = 0usize;
        self.composer.extend(value.chars().take_while(|ch| {
            used = used.saturating_add(ch.len_utf8());
            used <= remaining
        }));
    }

    fn on_approval_request(&mut self, pending: PendingApproval) {
        // Ctrl+C can race with an approval being emitted. Never leave the turn blocked on a
        // request that arrived after shutdown started.
        if self.shutdown != ShutdownState::Active {
            let _ = pending.respond.send(Decision::Abort);
            return;
        }
        self.approval_detail = Some(ApprovalDetail::new(safe_terminal_text(&format!(
            "reason: {}\nrequest: {:?}",
            pending.request.reason, pending.request.kind
        ))));
        self.pending = Some(pending);
    }

    fn on_approval_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if let Some(detail) = &self.approval_detail {
                    detail.previous(1);
                }
                return;
            }
            KeyCode::Down => {
                if let Some(detail) = &self.approval_detail {
                    detail.next(1);
                }
                return;
            }
            KeyCode::PageUp => {
                if let Some(detail) = &self.approval_detail {
                    detail.previous(10);
                }
                return;
            }
            KeyCode::PageDown => {
                if let Some(detail) = &self.approval_detail {
                    detail.next(10);
                }
                return;
            }
            KeyCode::Home => {
                if let Some(detail) = &self.approval_detail {
                    detail.home();
                }
                return;
            }
            KeyCode::End => {
                if let Some(detail) = &self.approval_detail {
                    detail.end();
                }
                return;
            }
            _ => {}
        }
        let decision = match key.code {
            KeyCode::Char('y') => Some(Decision::Approve),
            KeyCode::Char('a') => Some(Decision::ApproveForSession),
            KeyCode::Char('d') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision
            && let Some(pending) = self.pending.take()
        {
            self.approval_detail = None;
            let _ = pending.respond.send(decision);
        }
    }

    fn submit(&mut self) {
        let text = self.composer.trim().to_string();
        if text.is_empty() || self.running {
            return;
        }
        self.composer.clear();
        if let Some(cmd) = text.strip_prefix('/') {
            self.handle_slash(cmd);
            return;
        }
        self.push_entry(Entry::User(text.clone()));
        self.follow = true;
        self.start_turn(text, false);
    }

    /// Spawn a turn (execute or plan mode), moving the session + rollout into the task.
    fn start_turn(&mut self, text: String, plan: bool) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let mut rollout = self.rollout.take();
        self.running = true;
        self.turn_complete_seen = false;
        self.reasoning = None;
        self.active_tool = None;
        self.stream_retry = None;
        let cancellation = TurnCancellation::new();
        self.turn_cancellation = Some(cancellation.clone());
        let agent = Arc::clone(&self.agent);
        self.turn_handle = Some(tokio::spawn(async move {
            let turn = AssertUnwindSafe(async {
                if plan {
                    agent
                        .run_plan_turn_cancellable(&mut session, &text, &mut rollout, &cancellation)
                        .await;
                } else {
                    agent
                        .run_turn_cancellable(&mut session, &text, &mut rollout, &cancellation)
                        .await;
                }
            })
            .catch_unwind()
            .await;
            TurnOutcome {
                session,
                rollout,
                panic: turn.err().map(|payload| panic_message(payload.as_ref())),
            }
        }));
    }

    fn handle_slash(&mut self, cmd: &str) {
        let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match name {
            "help" | "?" => {
                self.push_entry(Entry::Info(
                    "commands: /plan <task>  ·  /skills [name]  ·  /tools [web|x|code] [on|off]  ·  /undo  ·  /clear  ·  /quit".to_string(),
                ));
                if !self.project_commands.is_empty() {
                    let commands = self
                        .project_commands
                        .iter()
                        .map(|command| format!("/{}", command.name))
                        .collect::<Vec<_>>()
                        .join("  ·  ");
                    self.push_entry(Entry::Info(format!(
                        "project commands: {commands} · arguments are appended to the template"
                    )));
                }
            }
            "quit" | "exit" | "q" => self.request_quit(),
            "clear" => {
                self.transcript.clear();
                self.transcript_bytes = 0;
                self.omitted_entries = 0;
            }
            "undo" => self.undo(),
            "skills" => self.show_skills(rest.trim()),
            "tools" => self.handle_server_tools(rest.trim()),
            "plan" => {
                let task = rest.trim();
                if task.is_empty() {
                    self.push_entry(Entry::Info("usage: /plan <task>".to_string()));
                } else {
                    self.push_entry(Entry::User(format!("/plan {task}")));
                    self.follow = true;
                    self.start_turn(task.to_string(), true);
                }
            }
            other => {
                if !self.run_project_command(other, rest) {
                    self.push_entry(Entry::Info(format!(
                        "unknown command: /{other} (try /help)"
                    )));
                }
            }
        }
    }

    fn run_project_command(&mut self, name: &str, arguments: &str) -> bool {
        let Some(command) = self
            .project_commands
            .iter()
            .find(|command| command.name == name)
            .cloned()
        else {
            return false;
        };
        let arguments = arguments.trim();
        let display = if arguments.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arguments}")
        };
        self.push_entry(Entry::User(display));
        self.follow = true;
        self.start_turn(commands::expand(&command, arguments), false);
        true
    }

    fn handle_server_tools(&mut self, input: &str) {
        let mut parts = input.split_whitespace();
        let tool_name = parts.next();
        let toggle = parts.next();
        if parts.next().is_some() || tool_name.is_some() != toggle.is_some() {
            self.push_entry(Entry::Info(
                "usage: /tools  OR  /tools <web|x|code> <on|off>".to_string(),
            ));
            return;
        }

        let Some(tool_name) = tool_name else {
            let enabled = self
                .session
                .as_ref()
                .map(|session| session.config.enabled_server_tools.clone())
                .unwrap_or_default();
            self.push_entry(Entry::Info(
                "xAI-HOSTED TOOLS · optional · metered separately by xAI".to_string(),
            ));
            for (name, tool) in server_tools() {
                self.push_entry(Entry::ServerTool {
                    name: name.to_string(),
                    enabled: enabled.contains(&tool),
                });
            }
            return;
        };

        let Some(tool) = parse_server_tool(tool_name) else {
            self.push_entry(Entry::Info(format!(
                "unknown xAI tool: {tool_name} · choose web, x, or code"
            )));
            return;
        };
        let enabled = match toggle {
            Some("on" | "enable" | "enabled") => true,
            Some("off" | "disable" | "disabled") => false,
            _ => {
                self.push_entry(Entry::Info(
                    "tool state must be on or off · example: /tools web on".to_string(),
                ));
                return;
            }
        };
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if enabled {
            session.config.enabled_server_tools.insert(tool);
        } else {
            session.config.enabled_server_tools.remove(&tool);
        }
        self.push_entry(Entry::ServerTool {
            name: server_tool_name(tool).to_string(),
            enabled,
        });
        self.push_entry(Entry::Info(
            "applies to the next turn · xAI-hosted calls are metered separately".to_string(),
        ));
    }

    fn show_skills(&mut self, requested: &str) {
        if self.available_skills.is_empty() {
            self.push_entry(Entry::Info(
                "No project skills found · add .grokforge/skills/<name>/SKILL.md".to_string(),
            ));
            return;
        }

        let matching: Vec<Entry> = self
            .available_skills
            .iter()
            .filter(|skill| requested.is_empty() || skill.name == requested)
            .map(|skill| Entry::Skill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                path: format!(".grokforge/skills/{}/SKILL.md", skill.name),
            })
            .collect();
        if matching.is_empty() {
            self.push_entry(Entry::Info(format!(
                "Unknown skill · {requested} · run /skills to list available workflows"
            )));
            return;
        }

        if requested.is_empty() {
            self.push_entry(Entry::Info(format!(
                "PROJECT SKILLS · {} available · /skills <name> inspects one",
                matching.len()
            )));
        }
        for entry in matching {
            self.push_entry(entry);
        }
    }

    /// Undo the last agent commit for this session (git, from the host process).
    fn undo(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let root = session.config.workspace_root.clone();
        let id = session.id;
        self.follow = true;
        self.scroll = 0;
        self.running = true;
        self.undo_handle =
            Some(tokio::task::spawn_blocking(
                move || match grokforge_git::Git::discover(&root) {
                    Some(git) => match git.undo_last(id) {
                        Ok(Some(msg)) => Entry::Git(format!("undo: {msg}")),
                        Ok(None) => Entry::Info("nothing to undo for this session".to_string()),
                        Err(error) => Entry::Error(format!("undo failed: {error}")),
                    },
                    None => Entry::Info("not a git repository".to_string()),
                },
            ));
    }

    #[allow(clippy::too_many_lines)]
    fn on_agent_event(&mut self, msg: EventMsg) {
        match msg {
            EventMsg::AgentMessageDelta { delta } => {
                self.flush_reasoning();
                self.stream_retry = None;
                append_bounded(
                    self.streaming.get_or_insert_with(String::new),
                    &delta,
                    MAX_ENTRY_BYTES,
                );
            }
            EventMsg::AgentMessageDone { text } => {
                self.flush_reasoning();
                self.stream_retry = None;
                self.streaming = None;
                self.push_entry(Entry::Assistant(text));
            }
            EventMsg::ReasoningDelta { delta } => {
                append_bounded(
                    self.reasoning.get_or_insert_with(String::new),
                    &delta,
                    MAX_ENTRY_BYTES,
                );
            }
            EventMsg::ToolCallBegin {
                name,
                args_preview,
                sandboxed,
                ..
            } => {
                self.flush_reasoning();
                self.stream_retry = None;
                let text = humanize_tool_call(&name, &args_preview);
                self.active_tool = text
                    .split_whitespace()
                    .next()
                    .map(|name| bounded_text(&safe_terminal_line(name), 48));
                self.push_entry(Entry::Tool {
                    text,
                    sandboxed: Some(sandboxed),
                });
            }
            EventMsg::ToolCallEnd {
                ok,
                summary,
                denial,
                ..
            } => {
                self.active_tool = None;
                self.push_entry(Entry::ToolResult {
                    ok,
                    text: summary,
                    denial,
                });
            }
            EventMsg::ApprovalResolved {
                summary,
                decision,
                auto,
            } => {
                let approved = decision.starts_with("Approve");
                self.push_entry(Entry::Approval {
                    summary,
                    decision,
                    approved,
                    auto,
                });
            }
            EventMsg::LedgerAppended(entry) => {
                self.ledger_sources = self.ledger_sources.saturating_add(1);
                self.ledger_bytes = self.ledger_bytes.saturating_add(entry.bytes);
                self.ledger_redactions = self.ledger_redactions.saturating_add(entry.redactions);
                if entry.redactions > 0 {
                    self.push_entry(Entry::Info(format!(
                        "privacy · {} secret(s) redacted from {}",
                        entry.redactions, entry.source
                    )));
                }
            }
            EventMsg::TokenUsage { usage } => {
                self.usage.add(usage);
            }
            EventMsg::StreamRetrying { attempt, reason } => {
                self.stream_retry = Some(attempt);
                self.push_entry(Entry::Retry { attempt, reason });
            }
            EventMsg::Committed { sha, message } => {
                let short = &sha[..sha.len().min(8)];
                self.push_entry(Entry::Git(format!("committed {short}  {message}")));
            }
            EventMsg::Error { message, .. } => {
                self.active_tool = None;
                self.stream_retry = None;
                self.push_entry(Entry::Error(message));
            }
            EventMsg::TurnComplete { .. } => {
                self.flush_reasoning();
                self.active_tool = None;
                self.stream_retry = None;
                // All earlier events from this turn are ahead of TurnComplete in the FIFO. Only
                // advertise idle after both this marker and the task join have been observed.
                self.turn_complete_seen = true;
                if self.turn_handle.is_none() {
                    self.running = false;
                    self.turn_cancellation = None;
                    self.finish_quit_if_quiescent();
                }
            }
            EventMsg::SessionConfigured { .. }
            | EventMsg::TurnStarted { .. }
            | EventMsg::ToolOutputDelta { .. }
            | EventMsg::ApprovalRequested(_)
            | EventMsg::ShutdownComplete => {}
        }
    }

    fn flush_reasoning(&mut self) {
        if let Some(reasoning) = self.reasoning.take()
            && !reasoning.trim().is_empty()
        {
            self.push_entry(Entry::Reasoning(reasoning));
        }
    }

    fn finish_turn(&mut self, result: Result<TurnOutcome, tokio::task::JoinError>) {
        self.turn_handle.take();
        self.streaming = None;
        match result {
            Ok(outcome) => {
                self.session = Some(outcome.session);
                self.rollout = outcome.rollout;
                if let Some(message) = outcome.panic {
                    self.push_entry(Entry::Error(format!(
                        "turn task panicked: {message}; session closed so recovery can repair any interrupted tool call"
                    )));
                    // Continuing with the in-memory transcript could replay a durably recorded
                    // function call whose output was lost in the panic. Closing releases the
                    // rollout lock; the next `resume` atomically repairs outstanding calls.
                    self.running = false;
                    self.turn_cancellation = None;
                    self.request_quit();
                } else if self.turn_complete_seen {
                    self.running = false;
                    self.turn_cancellation = None;
                }
            }
            Err(error) => {
                // A cancelled join loses ownership of the moved session; exit cleanly rather
                // than trapping the UI forever in a fake "working" state.
                self.push_entry(Entry::Error(format!("turn task failed: {error}")));
                self.running = false;
                self.turn_cancellation = None;
                self.request_quit();
            }
        }
        self.finish_quit_if_quiescent();
    }

    fn finish_undo(&mut self, result: Result<Entry, tokio::task::JoinError>) {
        self.undo_handle.take();
        self.running = false;
        match result {
            Ok(entry) => self.push_entry(entry),
            Err(error) => self.push_entry(Entry::Error(format!("undo task failed: {error}"))),
        }
        self.finish_quit_if_quiescent();
    }

    fn render(&self, f: &mut ratatui::Frame) {
        let area = f.area();
        if area.width == 0 || area.height == 0 {
            return;
        }

        f.render_widget(
            Block::default().style(Style::default().bg(CANVAS).fg(TEXT)),
            area,
        );

        // The spacious layout carries product identity and context. As height disappears, chrome
        // drops away before transcript/composer space does, so even a split-pane terminal remains
        // usable rather than becoming a stack of borders.
        let header_height = if area.height >= 16 && area.width >= 48 {
            3
        } else {
            u16::from(area.height >= 8)
        };
        let status_height = u16::from(area.height >= 5);
        let remaining = area
            .height
            .saturating_sub(header_height)
            .saturating_sub(status_height);
        let composer_height = if remaining >= 4 {
            3
        } else {
            u16::from(remaining >= 2)
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(0),
                Constraint::Length(composer_height),
                Constraint::Length(status_height),
            ])
            .split(area);

        self.render_header(chunks[0], f);
        self.render_transcript(chunks[1], f);
        self.render_composer(chunks[2], f);
        self.render_status(chunks[3], f);

        if let (Some(pending), Some(detail)) = (&self.pending, &self.approval_detail) {
            render_approval_modal(area, f, pending, detail);
        }
        apply_display_fallback(f, area, self.display_mode);
    }

    fn activity_state(&self) -> (String, Color) {
        if self.pending.is_some() {
            ("● APPROVAL".to_string(), WARNING)
        } else if let Some(attempt) = self.stream_retry {
            (format!("↻ RETRY {attempt}"), WARNING)
        } else if let Some(tool) = &self.active_tool {
            (format!("● {}", compact_preview(tool, 18)), TOOL)
        } else if self.reasoning.is_some() {
            ("◇ THINKING".to_string(), MUTED)
        } else if self.running {
            ("● WORKING".to_string(), ACCENT)
        } else {
            ("● READY".to_string(), SUCCESS)
        }
    }

    fn render_header(&self, area: Rect, f: &mut ratatui::Frame) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let mut block = Block::default().style(Style::default().bg(SURFACE).fg(TEXT));
        if area.height > 1 {
            block = block
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER));
        }
        f.render_widget(block, area);

        let content = horizontal_inset(area, u16::from(area.width >= 24));
        let brand = Line::from(vec![
            Span::styled(
                "◢ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "GROKFORGE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(brand.clone()), first_row(content));

        let state = self.activity_state();
        let right = if area.height > 1 {
            Line::from(vec![
                Span::styled("GROK  /  ", Style::default().fg(FAINT)),
                Span::styled(
                    safe_terminal_line(&self.status_model),
                    Style::default().fg(MUTED),
                ),
                Span::raw("  "),
            ])
        } else {
            Line::from(vec![
                Span::styled(state.0.clone(), Style::default().fg(state.1)),
                Span::raw(" "),
            ])
        };
        if content.width
            >= u16::try_from(
                brand
                    .width()
                    .saturating_add(right.width())
                    .saturating_add(4),
            )
            .unwrap_or(u16::MAX)
        {
            f.render_widget(
                Paragraph::new(right).alignment(Alignment::Right),
                first_row(content),
            );
        }

        if area.height > 2 {
            let tagline = Line::from(vec![
                Span::styled(
                    "Make Grok great in the terminal.",
                    Style::default().fg(MUTED),
                ),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled("forge boldly", Style::default().fg(ACCENT_SOFT)),
            ]);
            let row = Rect::new(content.x, content.y.saturating_add(1), content.width, 1);
            f.render_widget(Paragraph::new(tagline.clone()), row);
            let state_line = Line::from(vec![
                Span::styled(state.0, Style::default().fg(state.1)),
                Span::raw("  "),
            ]);
            if content.width
                >= u16::try_from(
                    tagline
                        .width()
                        .saturating_add(state_line.width())
                        .saturating_add(4),
                )
                .unwrap_or(u16::MAX)
            {
                f.render_widget(Paragraph::new(state_line).alignment(Alignment::Right), row);
            }
        }
    }

    fn render_transcript(&self, area: Rect, f: &mut ratatui::Frame) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        f.render_widget(
            Block::default().style(Style::default().bg(CANVAS).fg(TEXT)),
            area,
        );

        let gutter = if area.width >= 72 {
            4
        } else {
            u16::from(area.width >= 28)
        };
        let content_area = horizontal_inset(area, gutter);
        let content_width = content_area.width.max(1);
        if self.transcript.is_empty() && self.streaming.is_none() && self.reasoning.is_none() {
            render_welcome(content_area, f);
            return;
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        // Keep redraw work and Paragraph's u16 scroll range bounded even when the retained
        // transcript is several MiB. The durable rollout still contains the complete history.
        // Only the short reasoning preview below is rendered live. Reserving the whole reasoning
        // buffer could evict the entire transcript even though almost none of it reaches screen.
        let live_bytes = self
            .streaming
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(
                self.reasoning
                    .as_deref()
                    .map_or(0, |reasoning| compact_preview(reasoning, 240).len()),
            );
        let mut byte_budget =
            MAX_RENDER_TRANSCRIPT_BYTES.saturating_sub(live_bytes.min(MAX_RENDER_TRANSCRIPT_BYTES));
        let mut visible_start = self.transcript.len();
        let mut visible_entries = 0usize;
        while visible_start > 0 && visible_entries < MAX_RENDER_TRANSCRIPT_ENTRIES {
            let candidate = visible_start - 1;
            let bytes = entry_bytes(&self.transcript[candidate]);
            if bytes > byte_budget {
                break;
            }
            byte_budget -= bytes;
            visible_start = candidate;
            visible_entries += 1;
        }
        let render_omitted = self.omitted_entries.saturating_add(visible_start);
        if render_omitted > 0 {
            push_entry_lines(
                &mut lines,
                &Entry::Info(format!(
                    "… {render_omitted} earlier transcript item(s) omitted from this view; full history remains in the session rollout …"
                )),
                content_width,
            );
        }
        for entry in &self.transcript[visible_start..] {
            push_entry_lines(&mut lines, entry, content_width);
        }
        if let Some(reasoning) = &self.reasoning {
            lines.push(Line::from(vec![
                Span::styled("◇  THINKING  ", Style::default().fg(FAINT)),
                Span::styled(compact_preview(reasoning, 240), Style::default().fg(MUTED)),
                Span::styled("  ●", Style::default().fg(ACCENT)),
            ]));
        }
        if let Some(streaming) = &self.streaming {
            push_role_header(&mut lines, "◆", "GROK", ACCENT, Some("● GENERATING"));
            let streaming = safe_terminal_text(streaming);
            push_body(&mut lines, &streaming, TEXT, content_width);
        }

        let viewport = content_area.height;
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = u16::try_from(para.line_count(content_width)).unwrap_or(u16::MAX);
        let max_scroll = total.saturating_sub(viewport);
        let scroll = if self.follow {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll)
        };

        f.render_widget(para.scroll((scroll, 0)), content_area);

        if !self.follow && area.width >= 28 {
            let label = format!(" ↑ {}  ·  End: latest ", self.scroll);
            let width = u16::try_from(Line::from(label.as_str()).width())
                .unwrap_or(area.width)
                .min(area.width);
            let badge = Rect::new(area.right().saturating_sub(width), area.y, width, 1);
            f.render_widget(
                Paragraph::new(label).style(Style::default().fg(MUTED).bg(SURFACE_RAISED)),
                badge,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn render_status(&self, area: Rect, f: &mut ratatui::Frame) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        f.render_widget(
            Block::default().style(Style::default().bg(SURFACE_RAISED).fg(TEXT)),
            area,
        );

        let (indicator, state_color) = self.activity_state();
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                indicator,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if area.width >= 24 {
            spans.extend([
                Span::styled("  │  ", Style::default().fg(FAINT)),
                Span::styled(
                    safe_terminal_line(&self.status_model),
                    Style::default().fg(TEXT),
                ),
            ]);
        }
        if area.width >= 46 {
            spans.extend([
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    safe_terminal_line(&self.status_preset).to_uppercase(),
                    Style::default().fg(MUTED),
                ),
            ]);
        }
        let used_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.output_tokens);
        if used_tokens > 0 && area.width >= 70 {
            spans.extend([
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    format!(
                        "tok {} · cache {}%",
                        compact_count(used_tokens),
                        cache_percent(self.usage)
                    ),
                    Style::default().fg(MUTED),
                ),
            ]);
        }
        if self.ledger_sources > 0 && area.width >= 96 {
            let redactions = if self.ledger_redactions > 0 {
                format!("/{}r", self.ledger_redactions)
            } else {
                String::new()
            };
            spans.extend([
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    format!(
                        "↑{}/{}{}",
                        self.ledger_sources,
                        compact_bytes(self.ledger_bytes),
                        redactions
                    ),
                    Style::default().fg(USER),
                ),
            ]);
        }
        if !self.available_skills.is_empty() && area.width >= 116 {
            spans.extend([
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    format!("✦ {} skills", self.available_skills.len()),
                    Style::default().fg(ACCENT_SOFT),
                ),
            ]);
        }
        if self.omitted_entries > 0 && (area.width >= 136 || (area.width >= 72 && used_tokens == 0))
        {
            spans.extend([
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    format!("{} history hidden", self.omitted_entries),
                    Style::default().fg(MUTED),
                ),
            ]);
        }
        let status = Line::from(spans);
        let status_width = status.width();
        f.render_widget(Paragraph::new(status), area);

        if area.width >= 94 {
            let hints = Line::from(vec![
                Span::styled("↵", Style::default().fg(ACCENT_SOFT)),
                Span::styled(" send   ", Style::default().fg(MUTED)),
                Span::styled("↑↓", Style::default().fg(TEXT)),
                Span::styled(" scroll   ", Style::default().fg(MUTED)),
                Span::styled("/help", Style::default().fg(TEXT)),
                Span::styled("   Ctrl+C quit  ", Style::default().fg(MUTED)),
            ]);
            if usize::from(area.width)
                >= status_width.saturating_add(hints.width()).saturating_add(2)
            {
                f.render_widget(Paragraph::new(hints).alignment(Alignment::Right), area);
            }
        }
    }

    fn render_composer(&self, area: Rect, f: &mut ratatui::Frame) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let compact = area.height < 3;
        let prefix = if compact { " › " } else { " ›  " };
        let prefix_width = u16::try_from(Line::from(prefix).width()).unwrap_or(0);
        let border_width = if compact { 0 } else { 2 };
        let available = area
            .width
            .saturating_sub(border_width)
            .saturating_sub(prefix_width);
        let display = composer_tail(&safe_terminal_text(&self.composer), usize::from(available));
        let content = if self.composer.is_empty() {
            Span::styled(
                if self.running {
                    "Grok is working · draft your next message…"
                } else {
                    "Ask Grok · describe a change, paste an error, or type /help"
                },
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )
        } else {
            Span::styled(display.as_str(), Style::default().fg(TEXT))
        };
        let mut block = Block::default().style(Style::default().bg(SURFACE).fg(TEXT));
        if !compact {
            let border_color = if self.pending.is_some() {
                WARNING
            } else {
                ACCENT
            };
            block = block
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .title(Line::from(Span::styled(
                    " PROMPT ",
                    Style::default()
                        .fg(ACCENT_SOFT)
                        .add_modifier(Modifier::BOLD),
                )));
        }
        let para = Paragraph::new(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            content,
        ]))
        .block(block);
        f.render_widget(para, area);
        if self.pending.is_none() && area.width > border_width {
            let composer_width = u16::try_from(Line::from(display.as_str()).width())
                .unwrap_or(u16::MAX)
                .min(available);
            let max_x = area.right().saturating_sub(u16::from(!compact));
            let x = area
                .x
                .saturating_add(u16::from(!compact))
                .saturating_add(prefix_width)
                .saturating_add(composer_width)
                .min(max_x);
            let y = area.y.saturating_add(u16::from(!compact));
            f.set_cursor_position((x, y.min(area.bottom().saturating_sub(1))));
        }
    }
}

async fn wait_for_turn(
    handle: &mut Option<JoinHandle<TurnOutcome>>,
) -> Result<TurnOutcome, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_undo(
    handle: &mut Option<JoinHandle<Entry>>,
) -> Result<Entry, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn resumed_transcript_tail(history: &[ResponseItem]) -> (Vec<Entry>, usize) {
    let mut tail = Vec::new();
    let mut bytes = 0usize;
    let entry_budget = MAX_TRANSCRIPT_ENTRIES.saturating_sub(2);
    let byte_budget = MAX_TRANSCRIPT_BYTES.saturating_sub(64 * 1024);
    for (scanned, item) in history.iter().rev().enumerate() {
        if scanned >= MAX_TRANSCRIPT_ENTRIES.saturating_mul(4)
            || tail.len() >= entry_budget
            || bytes >= byte_budget
        {
            break;
        }
        let Some(entry) = entry_from_history(item).map(bounded_entry) else {
            continue;
        };
        let entry_len = entry_bytes(&entry);
        if !tail.is_empty() && bytes.saturating_add(entry_len) > byte_budget {
            break;
        }
        bytes = bytes.saturating_add(entry_len);
        tail.push(entry);
    }
    tail.reverse();
    let omitted = history.len().saturating_sub(tail.len());
    (tail, omitted)
}

fn entry_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::User(text)
        | Entry::Assistant(text)
        | Entry::Reasoning(text)
        | Entry::Tool { text, .. }
        | Entry::ToolResult { text, .. }
        | Entry::Git(text)
        | Entry::Error(text)
        | Entry::Info(text) => text.len(),
        Entry::Approval {
            summary, decision, ..
        } => summary.len().saturating_add(decision.len()),
        Entry::Retry { reason, .. } => reason.len(),
        Entry::Skill {
            name,
            description,
            path,
        } => name
            .len()
            .saturating_add(description.len())
            .saturating_add(path.len()),
        Entry::ServerTool { name, .. } => name.len(),
    }
}

fn bounded_entry(entry: Entry) -> Entry {
    match entry {
        Entry::User(text) => Entry::User(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Assistant(text) => Entry::Assistant(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Reasoning(text) => Entry::Reasoning(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Tool { text, sandboxed } => Entry::Tool {
            text: bounded_text(&text, MAX_ENTRY_BYTES),
            sandboxed,
        },
        Entry::ToolResult { ok, text, denial } => Entry::ToolResult {
            ok,
            text: bounded_text(&text, MAX_ENTRY_BYTES),
            denial,
        },
        Entry::Approval {
            summary,
            decision,
            approved,
            auto,
        } => Entry::Approval {
            summary: bounded_text(&summary, MAX_ENTRY_BYTES / 2),
            decision: bounded_text(&decision, MAX_ENTRY_BYTES / 2),
            approved,
            auto,
        },
        Entry::Retry { attempt, reason } => Entry::Retry {
            attempt,
            reason: bounded_text(&reason, MAX_ENTRY_BYTES),
        },
        Entry::Skill {
            name,
            description,
            path,
        } => Entry::Skill {
            name: bounded_text(&name, 1_024),
            description: bounded_text(&description, MAX_ENTRY_BYTES.saturating_sub(5_120)),
            path: bounded_text(&path, 4_096),
        },
        Entry::ServerTool { name, enabled } => Entry::ServerTool {
            name: bounded_text(&name, 1_024),
            enabled,
        },
        Entry::Git(text) => Entry::Git(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Error(text) => Entry::Error(bounded_text(&text, MAX_ENTRY_BYTES)),
        Entry::Info(text) => Entry::Info(bounded_text(&text, MAX_ENTRY_BYTES)),
    }
}

fn bounded_text(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    let marker = "\n… [entry truncated in TUI; full value remains in rollout] …\n";
    let payload = cap.saturating_sub(marker.len());
    let mut head_end = payload / 2;
    while head_end > 0 && !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len().saturating_sub(payload.saturating_sub(head_end));
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{marker}{}", &value[..head_end], &value[tail_start..])
}

fn append_bounded(target: &mut String, delta: &str, cap: usize) {
    if target.len() >= cap {
        return;
    }
    let remaining = cap.saturating_sub(target.len());
    if delta.len() <= remaining {
        target.push_str(delta);
        return;
    }
    let mut end = remaining;
    while end > 0 && !delta.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&delta[..end]);
    target.push_str("\n… [streaming display truncated; final value is retained in rollout] …");
}

fn entry_from_history(item: &ResponseItem) -> Option<Entry> {
    match item {
        ResponseItem::UserMessage { text, .. } => {
            Some(Entry::User(bounded_text(text, MAX_ENTRY_BYTES)))
        }
        ResponseItem::AssistantMessage { text } => {
            Some(Entry::Assistant(bounded_text(text, MAX_ENTRY_BYTES)))
        }
        ResponseItem::Reasoning { text } => {
            Some(Entry::Reasoning(bounded_text(text, MAX_ENTRY_BYTES)))
        }
        ResponseItem::ToolCall {
            name, arguments, ..
        } => Some(Entry::Tool {
            text: humanize_tool_call(name, arguments),
            sandboxed: None,
        }),
        ResponseItem::ToolResult {
            content, is_error, ..
        } => Some(Entry::ToolResult {
            ok: !is_error,
            text: bounded_text(content, MAX_ENTRY_BYTES),
            denial: None,
        }),
        ResponseItem::CompactionSummary { text, .. } => Some(Entry::Info(format!(
            "compacted history: {}",
            bounded_text(text, MAX_ENTRY_BYTES.saturating_sub(20))
        ))),
        ResponseItem::ProviderOutput { item } => entry_from_provider_output(item),
        ResponseItem::EncryptedReasoning { .. } | ResponseItem::CompactionCheckpoint { .. } => None,
    }
}

fn entry_from_provider_output(item: &serde_json::Value) -> Option<Entry> {
    match item.get("type").and_then(serde_json::Value::as_str) {
        Some("message") => {
            let mut text = String::new();
            for part in item
                .get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| {
                    part.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                })
            {
                if let Some(delta) = part.get("text").and_then(serde_json::Value::as_str) {
                    append_bounded(&mut text, delta, MAX_ENTRY_BYTES);
                }
            }
            (!text.is_empty()).then_some(Entry::Assistant(text))
        }
        Some("function_call") => {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>");
            let arguments = item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("{}");
            Some(Entry::Tool {
                text: humanize_tool_call(name, arguments),
                sandboxed: None,
            })
        }
        Some(kind) if kind.ends_with("_call") => Some(Entry::Tool {
            text: format!("{kind} [provider details retained in rollout]"),
            sandboxed: None,
        }),
        _ => None,
    }
}

fn horizontal_inset(area: Rect, amount: u16) -> Rect {
    let inset = amount.min(area.width / 2);
    Rect::new(
        area.x.saturating_add(inset),
        area.y,
        area.width.saturating_sub(inset.saturating_mul(2)),
        area.height,
    )
}

fn first_row(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, u16::from(area.height > 0))
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn apply_display_fallback(f: &mut ratatui::Frame, area: Rect, mode: DisplayMode) {
    if mode.color && !mode.ascii {
        return;
    }
    let buffer = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            if !mode.color {
                cell.set_fg(Color::Reset).set_bg(Color::Reset);
            }
            if mode.ascii
                && let Some(replacement) = ascii_ui_symbol(cell.symbol())
            {
                cell.set_symbol(replacement);
            }
        }
    }
}

fn ascii_ui_symbol(symbol: &str) -> Option<&'static str> {
    match symbol {
        "╭" | "╮" | "╰" | "╯" | "└" | "✓" => Some("+"),
        "─" => Some("-"),
        "│" => Some("|"),
        "◢" => Some("#"),
        "◆" | "◇" | "●" | "✦" | "⚙" => Some("*"),
        "○" => Some("o"),
        "▲" => Some("!"),
        "✗" | "×" => Some("x"),
        "↻" => Some("~"),
        "☁" => Some("@"),
        "⎇" => Some("G"),
        "›" | "↵" | "⇥" => Some(">"),
        "↑" => Some("^"),
        "↓" => Some("v"),
        "·" | "…" => Some("."),
        _ => None,
    }
}

fn render_welcome(area: Rect, f: &mut ratatui::Frame) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if area.width >= 58 && area.height >= 12 {
        let card = centered_fixed(
            area,
            area.width.saturating_sub(4).min(76),
            10.min(area.height.saturating_sub(2)),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(SURFACE).fg(TEXT))
            .title(Line::from(vec![
                Span::styled(" ◆ ", Style::default().fg(ACCENT)),
                Span::styled(
                    "READY TO FORGE ",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]));
        let inner = horizontal_inset(block.inner(card), 2);
        f.render_widget(block, card);
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("GrokForge is ready", Style::default().fg(TEXT)),
                Span::styled(
                    " and grounded in this workspace.",
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Start with a question, a bug, or a change you want shipped.",
                Style::default().fg(TEXT),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("›  ", Style::default().fg(USER)),
                Span::styled(
                    "Explain how this repository fits together",
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(vec![
                Span::styled("›  ", Style::default().fg(ACCENT)),
                Span::styled(
                    "/plan a safe, reviewable change",
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(vec![
                Span::styled("ENTER", Style::default().fg(ACCENT_SOFT)),
                Span::styled(" send   ·   ", Style::default().fg(FAINT)),
                Span::styled("/help", Style::default().fg(TEXT)),
                Span::styled(" commands", Style::default().fg(FAINT)),
            ]),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let compact = centered_fixed(area, area.width.min(40), area.height.min(4));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "◆ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "GROKFORGE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled("Ready to build.", Style::default().fg(MUTED))),
        Line::from(Span::styled(
            "Ask Grok about this workspace.",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "Enter sends  ·  /help",
            Style::default().fg(FAINT),
        )),
    ];
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), compact);
}

fn push_role_header(
    lines: &mut Vec<Line<'static>>,
    glyph: &'static str,
    role: &'static str,
    color: Color,
    badge: Option<&'static str>,
) {
    let mut spans = vec![
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            role,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(badge) = badge {
        spans.extend([
            Span::raw("  "),
            Span::styled(badge, Style::default().fg(MUTED)),
        ]);
    }
    lines.push(Line::from(spans));
}

fn push_body(lines: &mut Vec<Line<'static>>, text: &str, color: Color, width: u16) {
    let indent = " ".repeat(usize::from(width).saturating_sub(1).min(3));
    let body_width = usize::from(width).saturating_sub(indent.len()).max(1);
    for logical_line in text.split('\n') {
        for visual_line in hard_wrap_display_line(logical_line, body_width) {
            // Pre-wrapping gives every continuation the same hanging indent. Leaving this to
            // `Paragraph::wrap` would put continuation rows back at column zero.
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(visual_line, Style::default().fg(color)),
            ]));
        }
    }
}

fn hard_wrap_display_line(line: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width = 0usize;
    for ch in line.chars() {
        // Ratatui does not assign tabs a reliable cell width. Expand them before measuring so a
        // tab cannot make a manually wrapped continuation drift back into the gutter.
        let expanded = if ch == '\t' { "    " } else { "" };
        if expanded.is_empty() {
            push_wrapped_char(&mut rows, &mut row, &mut row_width, ch, width);
        } else {
            for space in expanded.chars() {
                push_wrapped_char(&mut rows, &mut row, &mut row_width, space, width);
            }
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn push_wrapped_char(
    rows: &mut Vec<String>,
    row: &mut String,
    row_width: &mut usize,
    ch: char,
    width: usize,
) {
    let ch_width = ch.width().unwrap_or(0);
    if !row.is_empty() && row_width.saturating_add(ch_width) > width {
        rows.push(std::mem::take(row));
        *row_width = 0;
    }
    row.push(ch);
    *row_width = row_width.saturating_add(ch_width);
}

#[allow(clippy::too_many_lines)]
fn push_entry_lines(lines: &mut Vec<Line<'static>>, entry: &Entry, width: u16) {
    match entry {
        Entry::User(text) => {
            push_role_header(lines, "›", "YOU", USER, None);
            let text = safe_terminal_text(text);
            push_body(lines, &text, TEXT, width);
        }
        Entry::Assistant(text) => {
            push_role_header(lines, "◆", "GROK", ACCENT, None);
            let text = safe_terminal_text(text);
            push_body(lines, &text, TEXT, width);
        }
        Entry::Reasoning(text) => {
            lines.push(Line::from(vec![
                Span::styled("◇  THINKING  ", Style::default().fg(FAINT)),
                Span::styled(compact_preview(text, 240), Style::default().fg(MUTED)),
            ]));
        }
        Entry::Tool { text, sandboxed } => {
            let text = safe_terminal_text(text);
            let (name, detail) = text.split_once(' ').unwrap_or((&text, ""));
            let detail = detail.trim_start();
            let mut spans = vec![
                Span::styled("⚙  ", Style::default().fg(TOOL)),
                Span::styled(
                    name.to_string(),
                    Style::default().fg(TOOL).add_modifier(Modifier::BOLD),
                ),
            ];
            match sandboxed {
                Some(true) => {
                    spans.push(Span::styled("  ● sandboxed", Style::default().fg(SUCCESS)));
                }
                Some(false) => {
                    spans.push(Span::styled("  ▲ host", Style::default().fg(WARNING)));
                }
                None => {}
            }
            spans.push(Span::styled(
                format!("  {detail}"),
                Style::default().fg(MUTED),
            ));
            lines.push(Line::from(spans));
        }
        Entry::ToolResult { ok, text, denial } => {
            let (glyph, color) = if *ok {
                ("✓", SUCCESS)
            } else {
                ("✗", DANGER)
            };
            let text = safe_terminal_text(text);
            let mut body = text.split('\n');
            let mut spans = vec![
                Span::styled("└─ ", Style::default().fg(FAINT)),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    body.next().unwrap_or_default().to_string(),
                    Style::default().fg(MUTED),
                ),
            ];
            if let Some(denial) = denial {
                spans.extend([
                    Span::styled("  ·  BLOCKED ", Style::default().fg(DANGER)),
                    Span::styled(denial_label(*denial), Style::default().fg(DANGER)),
                ]);
            }
            lines.push(Line::from(spans));
            let remaining = body.collect::<Vec<_>>().join("\n");
            if !remaining.is_empty() {
                push_body(lines, &remaining, MUTED, width);
            }
        }
        Entry::Approval {
            summary,
            decision,
            approved,
            auto,
        } => {
            let (glyph, color) = if *approved {
                ("✓", SUCCESS)
            } else {
                ("×", DANGER)
            };
            let mut spans = vec![
                Span::styled(format!("{glyph}  APPROVAL  "), Style::default().fg(color)),
                Span::styled(safe_terminal_line(decision), Style::default().fg(TEXT)),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(safe_terminal_line(summary), Style::default().fg(MUTED)),
            ];
            if *auto {
                spans.push(Span::styled("  AUTO", Style::default().fg(WARNING)));
            }
            lines.push(Line::from(spans));
        }
        Entry::Retry { attempt, reason } => {
            lines.push(Line::from(vec![
                Span::styled("↻  RETRY  ", Style::default().fg(WARNING)),
                Span::styled(format!("attempt {attempt}"), Style::default().fg(TEXT)),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(compact_preview(reason, 240), Style::default().fg(MUTED)),
            ]));
        }
        Entry::Skill {
            name,
            description,
            path,
        } => {
            lines.push(Line::from(vec![
                Span::styled("✦  SKILL  ", Style::default().fg(ACCENT_SOFT)),
                Span::styled(
                    safe_terminal_line(name),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]));
            push_body(lines, &safe_terminal_text(description), MUTED, width);
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(safe_terminal_line(path), Style::default().fg(FAINT)),
            ]));
        }
        Entry::ServerTool { name, enabled } => {
            let (state, color) = if *enabled {
                ("● ON", SUCCESS)
            } else {
                ("○ OFF", MUTED)
            };
            lines.push(Line::from(vec![
                Span::styled("☁  xAI TOOL  ", Style::default().fg(USER)),
                Span::styled(
                    safe_terminal_line(name),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {state}"), Style::default().fg(color)),
                Span::styled("  ·  metered", Style::default().fg(WARNING)),
            ]));
        }
        Entry::Git(text) => {
            lines.push(Line::from(vec![
                Span::styled("⎇  ", Style::default().fg(GIT)),
                Span::styled(
                    safe_terminal_text(text),
                    Style::default().fg(GIT).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        Entry::Error(text) => {
            lines.push(Line::from(vec![
                Span::styled(
                    "!  ERROR  ",
                    Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(safe_terminal_text(text), Style::default().fg(TEXT)),
            ]));
        }
        Entry::Info(text) => {
            lines.push(Line::from(vec![
                Span::styled("·  ", Style::default().fg(FAINT)),
                Span::styled(safe_terminal_text(text), Style::default().fg(MUTED)),
            ]));
        }
    }
    if !matches!(entry, Entry::Tool { .. }) {
        lines.push(Line::from(""));
    }
}

fn denial_label(denial: DenialClass) -> &'static str {
    match denial {
        DenialClass::FsWrite => "filesystem write",
        DenialClass::FsRead => "filesystem read",
        DenialClass::Network => "network",
        DenialClass::Signal => "signal",
    }
}

fn compact_preview(value: &str, max_chars: usize) -> String {
    let value = safe_terminal_line(value);
    let mut output: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn humanize_tool_call(name: &str, arguments: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let arg = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_str)
            .map(|value| compact_preview(value, 320))
    };
    let path = || arg("path").unwrap_or_else(|| ".".to_string());
    match name {
        "read_file" => format!("read  {}", path()),
        "write_file" => format!("write  {}", path()),
        "edit" => format!("edit  {}", path()),
        "list" => format!("list  {}", path()),
        "glob" => format!(
            "glob  {}",
            arg("pattern").unwrap_or_else(|| "<pattern>".to_string())
        ),
        "grep" => format!(
            "search  /{}/  in {}",
            arg("pattern").unwrap_or_else(|| "<pattern>".to_string()),
            path()
        ),
        "shell" => format!(
            "shell  $ {}",
            arg("command").unwrap_or_else(|| "<command>".to_string())
        ),
        "git_status" => "git  status".to_string(),
        "git_diff" => {
            let staged = parsed
                .as_ref()
                .and_then(|value| value.get("staged"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            format!("git  diff · {}", if staged { "staged" } else { "unstaged" })
        }
        "spawn_task" => format!(
            "agent  {}",
            arg("prompt").unwrap_or_else(|| "isolated subtask".to_string())
        ),
        _ => {
            let name = compact_preview(name, 64);
            let arguments = compact_preview(arguments, 360);
            if arguments.is_empty() || arguments == "{}" {
                name
            } else {
                format!("{name}  {arguments}")
            }
        }
    }
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        compact_scaled(value, 1_000_000, "m")
    } else if value >= 1_000 {
        compact_scaled(value, 1_000, "k")
    } else {
        value.to_string()
    }
}

fn compact_bytes(value: usize) -> String {
    let value = u64::try_from(value).unwrap_or(u64::MAX);
    if value >= 1024 * 1024 {
        compact_scaled(value, 1024 * 1024, "MB")
    } else if value >= 1024 {
        compact_scaled(value, 1024, "KB")
    } else {
        format!("{value}B")
    }
}

fn compact_scaled(value: u64, unit: u64, suffix: &str) -> String {
    let whole = value / unit;
    let tenth = value % unit / (unit / 10).max(1);
    if whole < 10 && tenth > 0 {
        format!("{whole}.{tenth}{suffix}")
    } else {
        format!("{whole}{suffix}")
    }
}

fn cache_percent(usage: Usage) -> u64 {
    if usage.input_tokens == 0 {
        return 0;
    }
    let percent = u128::from(usage.cached_tokens)
        .saturating_mul(100)
        .checked_div(u128::from(usage.input_tokens))
        .unwrap_or(0)
        .min(100);
    u64::try_from(percent).unwrap_or(100)
}

fn server_tools() -> [(&'static str, ServerTool); 3] {
    [
        ("web search", ServerTool::WebSearch),
        ("X search", ServerTool::XSearch),
        ("code interpreter", ServerTool::CodeInterpreter),
    ]
}

fn parse_server_tool(name: &str) -> Option<ServerTool> {
    match name {
        "web" | "web_search" | "web-search" => Some(ServerTool::WebSearch),
        "x" | "x_search" | "x-search" => Some(ServerTool::XSearch),
        "code" | "code_interpreter" | "code-interpreter" => Some(ServerTool::CodeInterpreter),
        _ => None,
    }
}

fn server_tool_name(tool: ServerTool) -> &'static str {
    match tool {
        ServerTool::WebSearch => "web search",
        ServerTool::XSearch => "X search",
        ServerTool::CodeInterpreter => "code interpreter",
    }
}

fn composer_tail(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let normalized = value.replace('\n', " ↵ ").replace('\t', " ⇥ ");
    if Line::from(normalized.as_str()).width() <= max_width {
        return normalized;
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut reversed = Vec::new();
    let mut width = 1usize;
    for ch in normalized.chars().rev() {
        let char_width = ch.width().unwrap_or(0);
        if width.saturating_add(char_width) > max_width {
            break;
        }
        reversed.push(ch);
        width = width.saturating_add(char_width);
    }
    reversed.reverse();
    format!("…{}", reversed.into_iter().collect::<String>())
}

#[allow(clippy::too_many_lines)]
fn render_approval_modal(
    area: Rect,
    f: &mut ratatui::Frame,
    pending: &PendingApproval,
    detail: &ApprovalDetail,
) {
    let modal = approval_sheet_rect(area);
    let backdrop = Rect::new(area.x, modal.y, area.width, modal.height);
    f.render_widget(Clear, backdrop);
    f.render_widget(
        Block::default().style(Style::default().bg(CANVAS).fg(TEXT)),
        backdrop,
    );
    f.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled(
                " ▲ ",
                Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "APPROVAL REQUIRED ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]))
        .border_style(Style::default().fg(WARNING))
        .style(Style::default().bg(SURFACE).fg(TEXT));
    let inner = horizontal_inset(block.inner(modal), u16::from(modal.width >= 12));
    f.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let reason_height = if inner.height >= 8 { 2 } else { 1 };
    let metadata_height = u16::from(inner.height >= 5);
    let controls_height = if inner.height >= 7 { 2 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(reason_height),
            Constraint::Length(metadata_height),
            Constraint::Min(1),
            Constraint::Length(controls_height),
        ])
        .split(inner);
    let detail_rows = chunks[2].height.max(1);
    let page = detail.page(chunks[2].width, detail_rows);
    let total = page
        .total
        .map_or_else(|| "?".to_string(), |total| total.to_string());

    let reason = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("WHY  ", Style::default().fg(WARNING)),
            Span::styled(
                safe_terminal_line(&pending.request.reason),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "GrokForge paused before crossing a safety boundary.",
            Style::default().fg(MUTED),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(reason, chunks[0]);

    if chunks[1].height > 0 {
        let status = Paragraph::new(Line::from(vec![
            Span::styled("REQUEST  ", Style::default().fg(TOOL)),
            Span::styled(
                format!(
                    "page {}/{total}  ·  bytes {}..{} / {}",
                    page.number, page.start, page.end, page.bytes
                ),
                Style::default().fg(MUTED),
            ),
        ]));
        f.render_widget(status, chunks[1]);
    }

    let body = Paragraph::new(page.body)
        .style(Style::default().fg(TEXT).bg(SURFACE_RAISED))
        .wrap(Wrap { trim: false });
    f.render_widget(body, chunks[2]);

    let controls = if chunks[3].width >= 58 && chunks[3].height >= 2 {
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" Esc ", Style::default().fg(TEXT).bg(DANGER)),
                Span::styled(" deny   ", Style::default().fg(MUTED)),
                Span::styled(" y ", Style::default().fg(CANVAS).bg(SUCCESS)),
                Span::styled(" approve once   ", Style::default().fg(MUTED)),
                Span::styled(" a ", Style::default().fg(CANVAS).bg(WARNING)),
                Span::styled(" approve for session", Style::default().fg(MUTED)),
            ]),
            Line::from(vec![
                Span::styled("↑↓", Style::default().fg(TEXT)),
                Span::styled(" page   ", Style::default().fg(MUTED)),
                Span::styled("PgUp/PgDn", Style::default().fg(TEXT)),
                Span::styled(" ×10   ", Style::default().fg(MUTED)),
                Span::styled("Home/End", Style::default().fg(TEXT)),
                Span::styled(" jump   ·   Esc denies", Style::default().fg(MUTED)),
            ]),
        ])
    } else if chunks[3].height >= 2 {
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Esc deny", Style::default().fg(DANGER)),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled("y approve", Style::default().fg(SUCCESS)),
            ]),
            Line::from(vec![
                Span::styled("a session", Style::default().fg(WARNING)),
                Span::styled("  ·  ↑↓ page", Style::default().fg(MUTED)),
            ]),
        ])
    } else {
        Paragraph::new(Line::from(vec![
            Span::styled("Esc deny", Style::default().fg(DANGER)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled("y yes", Style::default().fg(SUCCESS)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled("a all", Style::default().fg(WARNING)),
        ]))
    };
    f.render_widget(controls, chunks[3]);
}

fn approval_sheet_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let side_margin: u16 = if area.width >= 48 { 2 } else { 0 };
    let width = area
        .width
        .saturating_sub(side_margin.saturating_mul(2))
        .min(104);
    let available_height = area.height.saturating_sub(u16::from(area.height > 2));
    let target_height = area.height.saturating_mul(3) / 5;
    let height = target_height.clamp(8.min(available_height), 28.min(available_height));
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.bottom()
            .saturating_sub(height)
            .saturating_sub(u16::from(area.height > height)),
        width,
        height,
    )
}

/// Returns an exclusive UTF-8 byte boundary for one viewport-sized page.
fn approval_page_end(text: &str, start: usize, width: usize, rows: usize) -> usize {
    if start >= text.len() {
        return text.len();
    }

    let width = width.max(1);
    let rows = rows.max(1);
    let mut byte_limit = start.saturating_add(APPROVAL_PAGE_BYTES).min(text.len());
    while byte_limit > start && !text.is_char_boundary(byte_limit) {
        byte_limit -= 1;
    }

    let mut end = start;
    let mut row = 0usize;
    let mut column = 0usize;
    for (offset, ch) in text[start..byte_limit].char_indices() {
        let next = start + offset + ch.len_utf8();
        if ch == '\n' {
            end = next;
            row += 1;
            column = 0;
            if row >= rows {
                break;
            }
            continue;
        }

        let char_width = approval_char_width(ch);
        if column > 0 && column.saturating_add(char_width) > width {
            row += 1;
            if row >= rows {
                break;
            }
            column = 0;
        }
        end = next;
        column = column.saturating_add(char_width);
    }

    if end == start {
        // A one-column viewport can still consume a double-width character, and a very small
        // byte cap can still land before a multi-byte scalar. Always make forward progress.
        if let Some(ch) = text[start..].chars().next() {
            return start.saturating_add(ch.len_utf8()).min(text.len());
        }
    }
    end
}

fn approval_page_body(page: &str, width: usize) -> String {
    let width = width.max(1);
    let mut rendered = String::with_capacity(page.len());
    let mut column = 0usize;
    for (offset, ch) in page.char_indices() {
        if ch == '\n' {
            // A trailing newline was consumed as part of this page boundary. It should not add
            // an inaccessible blank display row below the viewport.
            if offset + ch.len_utf8() < page.len() {
                rendered.push('\n');
            }
            column = 0;
            continue;
        }

        let char_width = approval_char_width(ch);
        if column > 0 && column.saturating_add(char_width) > width {
            rendered.push('\n');
            column = 0;
        }
        if ch == '\t' {
            rendered.push_str("    ");
        } else {
            rendered.push(ch);
        }
        column = column.saturating_add(char_width);
    }
    rendered
}

fn approval_char_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        ch.width().unwrap_or(0)
    }
}

fn safe_terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            matches!(ch, '\n' | '\t')
                || (!ch.is_control() && !matches!(*ch as u32, 0x7f..=0x9f) && !is_bidi_control(*ch))
        })
        .collect()
}

fn safe_terminal_line(value: &str) -> String {
    safe_terminal_text(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::fmt::Write as _;
    use std::path::PathBuf;
    use std::sync::Arc;

    use grokforge_core::{Agent, Session, SessionConfig, ToolRegistry};
    use grokforge_protocol::{Decision, DenialClass, EventMsg, LedgerEntry, ResponseItem, Usage};
    use grokforge_sandbox::PassthroughRunner;
    use grokforge_xai::{ServerTool, XaiClient};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::text::Line;
    use tokio::sync::mpsc;

    use super::{App, ApprovalDetail, DisplayMode, Entry, MAX_COMPOSER_BYTES, TurnOutcome};
    use crate::approver::ChannelApprover;

    fn test_app() -> App {
        test_app_with_history(Vec::new())
    }

    fn test_app_with_history(history: Vec<ResponseItem>) -> App {
        test_app_in(PathBuf::from("/tmp"), history)
    }

    fn test_app_in(workspace: PathBuf, history: Vec<ResponseItem>) -> App {
        let client = XaiClient::new("http://127.0.0.1:1", "k").unwrap();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (approver, approvals_rx) = ChannelApprover::new();
        let agent = Arc::new(Agent::new(
            client,
            ToolRegistry::with_builtins(),
            Arc::new(PassthroughRunner),
            Arc::new(approver),
            events_tx,
        ));
        let session =
            Session::with_history(SessionConfig::new(workspace, "grok-build-0.1"), history);
        let mut app = App::new(
            agent,
            session,
            None,
            events_rx,
            approvals_rx,
            "grok-build-0.1".to_string(),
            "auto".to_string(),
        );
        // Unit tests assert the default rich presentation independently of the process that runs
        // them (CI commonly exports `TERM=dumb`). Fallback behavior has a dedicated test below.
        app.display_mode = DisplayMode {
            color: true,
            ascii: false,
        };
        app
    }

    fn buffer_text(app: &App, w: u16, h: u16) -> String {
        buffer_frame(app, w, h).replace('\n', "")
    }

    fn render_buffer(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_frame(app: &App, w: u16, h: u16) -> String {
        let buf = render_buffer(app, w, h);
        buf.content()
            .chunks(usize::from(w))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wrapped_body_continuations_keep_the_hanging_indent() {
        let mut lines = Vec::new();
        super::push_body(&mut lines, "abcdefghijklmnop", super::TEXT, 10);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["   abcdefg", "   hijklmn", "   op"]);
        assert!(lines.iter().all(|line| line.width() <= 10));
    }

    #[test]
    fn live_reasoning_reserves_only_its_visible_preview() {
        let mut app = test_app();
        app.transcript.push(Entry::Assistant(
            "TRANSCRIPT-MARKER remains visible while Grok thinks".to_string(),
        ));
        app.reasoning = Some("r".repeat(super::MAX_ENTRY_BYTES));

        let frame = buffer_frame(&app, 80, 24);
        assert!(frame.contains("TRANSCRIPT-MARKER"), "{frame}");
        assert!(frame.contains("THINKING"));
    }

    #[test]
    fn welcome_does_not_claim_an_unverified_network_connection() {
        let frame = buffer_frame(&test_app(), 80, 24);
        assert!(frame.contains("GrokForge is ready"));
        assert!(!frame.to_lowercase().contains("connected"));
    }

    #[test]
    fn structural_border_meets_non_text_contrast_target() {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(red, green, blue) = color else {
                panic!("expected RGB test color");
            };
            0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
        }
        let lighter = luminance(super::BORDER);
        let darker = luminance(super::SURFACE);
        let ratio = (lighter + 0.05) / (darker + 0.05);
        assert!(ratio >= 3.0, "border contrast was only {ratio:.2}:1");
    }

    #[test]
    fn plain_display_mode_uses_ascii_and_resets_every_color() {
        let mut app = test_app();
        app.display_mode = DisplayMode {
            color: false,
            ascii: true,
        };
        let buffer = render_buffer(&app, 80, 24);
        let frame = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            frame.is_ascii(),
            "plain frame contained non-ASCII UI: {frame}"
        );
        assert!(frame.contains("* READY TO FORGE"));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
    }

    #[test]
    fn wide_frames_preserve_the_visual_hierarchy() {
        let mut app = test_app();
        let welcome = buffer_frame(&app, 80, 24);
        assert!(welcome.contains("◆ READY TO FORGE"));
        assert!(welcome.contains("╭ PROMPT"));
        assert_eq!(welcome.lines().count(), 24);

        app.transcript.push(Entry::User(
            "Fix the flaky retry test in net/backoff.rs".to_string(),
        ));
        app.transcript.push(Entry::Assistant(
            "The flake comes from an unseeded RNG. I'll pin the seed and verify the suite."
                .to_string(),
        ));
        app.transcript.push(Entry::Tool {
            text: super::humanize_tool_call("edit", "{\"path\":\"net/backoff.rs\"}"),
            sandboxed: Some(true),
        });
        app.transcript.push(Entry::ToolResult {
            ok: true,
            text: "updated net/backoff.rs (+9 -4)".to_string(),
            denial: None,
        });
        app.transcript.push(Entry::Git(
            "committed a1f3c92 fix(net): seed jitter RNG".to_string(),
        ));
        let chat = buffer_frame(&app, 80, 24);
        assert!(chat.contains("› YOU"));
        assert!(chat.contains("◆ GROK"));
        assert!(chat.contains("⚙  edit  ● sandboxed  net/backoff.rs"));
        assert!(!chat.contains("{\"path\""));

        let (respond, _wait) = tokio::sync::oneshot::channel();
        app.pending = Some(crate::approver::PendingApproval {
            request: grokforge_protocol::ApprovalRequest {
                id: grokforge_protocol::ApprovalId::new(),
                call_id: None,
                kind: grokforge_protocol::ApprovalKind::ExecCommand {
                    command: vec!["cargo".into(), "test".into()],
                    cwd: "/tmp".into(),
                    sandbox: grokforge_protocol::SandboxMode::WorkspaceWrite,
                    escalation_of: None,
                },
                reason: "run the project test suite".to_string(),
            },
            respond,
        });
        app.approval_detail = Some(ApprovalDetail::new(
            "request: cargo test\ncwd: /tmp\nsandbox: workspace-write".to_string(),
        ));
        let approval = buffer_frame(&app, 80, 24);
        assert!(approval.contains("▲ APPROVAL REQUIRED"));
        assert!(approval.contains("GrokForge paused before crossing a safety boundary"));
        assert!(
            !approval.contains("╭ │"),
            "composer border leaked into sheet"
        );
    }

    #[test]
    fn project_capabilities_are_discoverable_and_server_tools_are_explicit_opt_ins() {
        let workspace = tempfile::tempdir().expect("workspace");
        let skill = workspace.path().join(".grokforge/skills/review");
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: Review changes carefully\n---\n# Review",
        )
        .expect("skill");
        let commands = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&commands).expect("commands directory");
        std::fs::write(commands.join("review.md"), "Review the current diff.").expect("command");

        let mut app = test_app_in(workspace.path().to_path_buf(), Vec::new());
        assert_eq!(app.available_skills.len(), 1);
        assert_eq!(app.project_commands.len(), 1);
        app.handle_slash("help");
        app.handle_slash("skills");
        app.handle_slash("tools web on");
        app.handle_slash("tools");

        assert!(
            app.session
                .as_ref()
                .expect("session")
                .config
                .enabled_server_tools
                .contains(&ServerTool::WebSearch)
        );
        let frame = buffer_frame(&app, 120, 40);
        assert!(frame.contains("project commands: /review"));
        assert!(frame.contains("✦  SKILL  review"));
        assert!(frame.contains("Review changes carefully"));
        assert!(frame.contains("☁  xAI TOOL  web search  ● ON"));
        assert!(frame.contains("metered"));
    }

    #[test]
    fn common_tool_calls_are_humanized_without_leaking_payloads() {
        assert_eq!(
            super::humanize_tool_call("read_file", "{\"path\":\"src/lib.rs\"}"),
            "read  src/lib.rs"
        );
        let write = super::humanize_tool_call(
            "write_file",
            "{\"path\":\"src/lib.rs\",\"content\":\"large private payload\"}",
        );
        assert_eq!(write, "write  src/lib.rs");
        assert!(!write.contains("private payload"));
        assert_eq!(
            super::humanize_tool_call("list", "{\"path\":\"crates\"}"),
            "list  crates"
        );
        assert_eq!(
            super::humanize_tool_call("glob", "{\"pattern\":\"**/*.rs\"}"),
            "glob  **/*.rs"
        );
        assert_eq!(
            super::humanize_tool_call("grep", "{\"pattern\":\"TODO\",\"path\":\"src\"}"),
            "search  /TODO/  in src"
        );
        assert_eq!(
            super::humanize_tool_call("git_diff", "{\"staged\":true}"),
            "git  diff · staged"
        );
    }

    #[test]
    fn renders_without_panicking_and_shows_chrome() {
        let app = test_app();
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("GROKFORGE"));
        assert!(text.contains("READY TO FORGE"));
        assert!(text.contains("grok-build-0.1"));
        assert!(text.contains("Ask Grok"));
    }

    #[test]
    fn renders_a_transcript_entry() {
        let mut app = test_app();
        app.transcript.push(Entry::User("hello there".to_string()));
        app.transcript
            .push(Entry::Assistant("hi from grok".to_string()));
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("hello there"));
        assert!(text.contains("hi from grok"));
    }

    #[test]
    fn protocol_activity_is_visible_in_transcript_and_status() {
        let mut app = test_app();
        app.running = true;
        app.on_agent_event(EventMsg::ReasoningDelta {
            delta: "Checking the retry path before editing.".to_string(),
        });
        assert!(buffer_text(&app, 100, 24).contains("THINKING"));

        app.on_agent_event(EventMsg::ToolCallBegin {
            call_id: grokforge_protocol::ToolCallId::new(),
            name: "shell".to_string(),
            args_preview: "{\"command\":\"cargo test retry\"}".to_string(),
            sandboxed: true,
        });
        app.on_agent_event(EventMsg::ToolCallEnd {
            call_id: grokforge_protocol::ToolCallId::new(),
            ok: false,
            summary: "network access denied".to_string(),
            denial: Some(DenialClass::Network),
        });
        app.on_agent_event(EventMsg::TokenUsage {
            usage: Usage {
                input_tokens: 1_000,
                cached_tokens: 500,
                output_tokens: 200,
                reasoning_tokens: 80,
            },
        });
        app.on_agent_event(EventMsg::LedgerAppended(
            LedgerEntry::new("src/retry.rs", 2_048, "tool read").with_redactions(1),
        ));
        app.on_agent_event(EventMsg::StreamRetrying {
            attempt: 2,
            reason: "upstream connection reset".to_string(),
        });
        app.on_agent_event(EventMsg::ApprovalResolved {
            summary: "`shell`".to_string(),
            decision: "Deny".to_string(),
            auto: false,
        });

        let frame = buffer_frame(&app, 168, 40);
        assert!(frame.contains("◇  THINKING"));
        assert!(frame.contains("⚙  shell  ● sandboxed  $ cargo test retry"));
        assert!(frame.contains("BLOCKED network"));
        assert!(frame.contains("↻  RETRY  attempt 2"));
        assert!(frame.contains("×  APPROVAL  Deny"));
        assert!(frame.contains("tok 1.2k · cache 50%"));
        assert!(frame.contains("↑1/2KB/1r"));
        assert!(frame.contains("secret(s) redacted"));
        assert!(!frame.contains("{\"command\""));
    }

    #[test]
    fn renders_approval_modal_when_pending() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        let (respond, _wait) = oneshot::channel();
        app.pending = Some(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/x".into(),
                },
                reason: "write a file".to_string(),
            },
            respond,
        });
        app.approval_detail = Some(ApprovalDetail::new(
            "reason: write a file\nrequest: WriteFile { path: /tmp/x }".to_string(),
        ));
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("APPROVAL REQUIRED"));
        assert!(text.contains("approve"));
    }

    #[test]
    fn narrow_approval_always_keeps_the_safe_deny_action_visible() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        for width in [12, 20, 30] {
            let mut app = test_app();
            let (respond, _wait) = oneshot::channel();
            app.pending = Some(crate::approver::PendingApproval {
                request: ApprovalRequest {
                    id: ApprovalId::new(),
                    call_id: None,
                    kind: ApprovalKind::WriteFile {
                        path: "/tmp/x".into(),
                    },
                    reason: "write a file".to_string(),
                },
                respond,
            });
            app.approval_detail = Some(ApprovalDetail::new("request: write /tmp/x".to_string()));
            let frame = buffer_frame(&app, width, 8);
            assert!(frame.contains("Esc deny"), "{width}-column frame:\n{frame}");
            assert_eq!(frame.lines().count(), 8);
        }
    }

    #[test]
    fn approval_pager_reaches_every_byte_across_many_lines() {
        let mut text = String::new();
        for index in 0..2_000 {
            writeln!(text, "line-{index:04} — approval context").expect("write string");
        }
        let detail = ApprovalDetail::new(text.clone());
        let mut expected_start = 0usize;
        let mut pages = 0usize;
        loop {
            let page = detail.page(24, 4);
            assert_eq!(page.start, expected_start);
            assert!(page.end > page.start || text.is_empty());
            assert!(page.body.lines().count() <= 4);
            for line in page.body.lines() {
                assert!(Line::from(line).width() <= 24);
            }
            pages += 1;
            if page.end == text.len() {
                assert_eq!(page.total, Some(pages));
                break;
            }
            expected_start = page.end;
            detail.next(1);
        }
        assert!(pages > 100);
    }

    #[test]
    fn approval_pager_hard_wraps_long_lines_and_end_reaches_the_tail() {
        let marker = "TAIL-APPROVAL-MARKER";
        let text = format!("{}\n{marker}", "界abcdef".repeat(20_000));
        let detail = ApprovalDetail::new(text.clone());

        let first = detail.page(17, 3);
        assert!(first.end <= super::APPROVAL_PAGE_BYTES);
        assert!(first.body.lines().count() <= 3);
        assert!(detail.pager.borrow().page_starts.len() <= 1);

        detail.end();
        let last = detail.page(17, 3);
        assert_eq!(last.end, text.len());
        assert!(
            last.body.replace('\n', "").contains(marker),
            "last page {}..{} of {} was {:?}",
            last.start,
            last.end,
            text.len(),
            last.body
        );
        assert!(last.total.is_some());
        for line in last.body.lines() {
            assert!(Line::from(line).width() <= 17);
        }
    }

    #[test]
    fn resumed_history_is_visible_in_the_transcript() {
        let app = test_app_with_history(vec![
            ResponseItem::user("prior question"),
            ResponseItem::assistant("prior answer"),
            ResponseItem::ProviderOutput {
                item: serde_json::json!({
                    "type": "message",
                    "content": [{"type": "output_text", "text": "raw assistant"}]
                }),
            },
            ResponseItem::ProviderOutput {
                item: serde_json::json!({
                    "type": "function_call",
                    "name": "read_file",
                    "arguments": "{\"path\":\"a.rs\"}"
                }),
            },
        ]);
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("prior question"));
        assert!(text.contains("prior answer"));
        assert!(text.contains("raw assistant"));
        assert!(text.contains("read"));
    }

    #[test]
    fn resumed_history_materializes_only_a_bounded_recent_tail() {
        let history = (0..10_000)
            .map(|index| ResponseItem::assistant(format!("history-{index}")))
            .collect();
        let app = test_app_with_history(history);
        assert!(app.transcript.len() <= super::MAX_TRANSCRIPT_ENTRIES);
        assert!(app.transcript_bytes <= super::MAX_TRANSCRIPT_BYTES);
        assert!(app.omitted_entries > 0);
        let text = buffer_text(&app, 80, 24);
        assert!(text.contains("hidden"));
        assert!(text.contains("history-9999"));
    }

    #[test]
    fn a_huge_resumed_entry_is_bounded_without_losing_its_tail() {
        let marker = "RECENT-TAIL";
        let app = test_app_with_history(vec![ResponseItem::assistant(format!(
            "{}{marker}",
            "x".repeat(super::MAX_ENTRY_BYTES * 4)
        ))]);
        let assistant = app
            .transcript
            .iter()
            .find_map(|entry| match entry {
                Entry::Assistant(text) => Some(text),
                _ => None,
            })
            .expect("assistant entry");
        assert!(assistant.len() <= super::MAX_ENTRY_BYTES);
        assert!(assistant.contains("entry truncated in TUI"));
        assert!(assistant.contains(marker));
    }

    #[tokio::test]
    async fn idle_waits_for_both_turn_complete_and_join() {
        use grokforge_protocol::{StopReason, TurnId};

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        app.turn_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        app.on_agent_event(EventMsg::TurnComplete {
            turn_id: TurnId::new(),
            stop: StopReason::EndTurn,
        });
        assert!(app.running, "join has not completed yet");
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.finish_turn(result);
        assert!(!app.running);

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        app.turn_handle = Some(tokio::spawn(async move {
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.finish_turn(result);
        assert!(app.running, "TurnComplete is still queued");
        app.on_agent_event(EventMsg::TurnComplete {
            turn_id: TurnId::new(),
            stop: StopReason::EndTurn,
        });
        assert!(!app.running);
    }

    #[test]
    fn up_from_follow_mode_scrolls_one_row_instead_of_jumping_to_top() {
        use crossterm::event::{KeyCode, KeyEvent};

        let mut app = test_app();
        for index in 0..50 {
            app.transcript
                .push(Entry::Assistant(format!("message-{index}")));
        }
        app.on_key(KeyEvent::from(KeyCode::Up));
        let text = buffer_text(&app, 60, 12);
        assert!(text.contains("message-49"));
        assert!(!text.contains("GrokForge — type"));
    }

    #[test]
    fn follow_mode_accounts_for_wrapped_rows_and_keeps_latest_visible() {
        let mut app = test_app();
        app.transcript.push(Entry::Assistant("word ".repeat(300)));
        app.transcript
            .push(Entry::Assistant("LATEST-MARKER".to_string()));
        let text = buffer_text(&app, 40, 12);
        assert!(text.contains("LATEST-MARKER"));
    }

    #[test]
    fn streaming_events_do_not_override_manual_scroll() {
        let mut app = test_app();
        app.follow = false;
        app.scroll = 12;
        app.on_agent_event(EventMsg::AgentMessageDelta {
            delta: "new streaming text".to_string(),
        });
        app.on_agent_event(EventMsg::ToolCallEnd {
            call_id: grokforge_protocol::ToolCallId::new(),
            ok: true,
            summary: "done".to_string(),
            denial: None,
        });
        assert!(!app.follow);
        assert_eq!(app.scroll, 12);
    }

    #[test]
    fn render_window_stays_bounded_and_keeps_the_latest_entry_visible() {
        let mut app = test_app();
        for index in 0..2_000 {
            app.push_entry(Entry::Assistant(format!(
                "entry-{index}: {}",
                "x".repeat(1_000)
            )));
        }
        app.push_entry(Entry::Assistant("LATESTQ".to_string()));

        let text = buffer_text(&app, 3, 12);
        assert!(text.contains('Q'));
        assert!(app.omitted_entries > 0 || app.transcript.len() > 100);
    }

    #[test]
    fn key_release_events_can_be_identified_and_ignored_by_the_loop() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let event = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        let mut app = test_app();
        app.on_terminal_key(event);
        assert!(app.composer.is_empty());
    }

    #[test]
    fn modified_chars_are_not_inserted_and_paste_is_bounded() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(app.composer.is_empty());

        app.on_paste(&format!(
            "{}\u{1b}\u{202e}",
            "a".repeat(MAX_COMPOSER_BYTES + 100)
        ));
        assert_eq!(app.composer.len(), MAX_COMPOSER_BYTES);
        assert!(!app.composer.contains('\u{1b}'));
        assert!(!app.composer.contains('\u{202e}'));
    }

    #[tokio::test]
    async fn ctrl_c_requests_cancellation_but_awaits_active_turn_quiescence() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        let session = app.session.take().expect("session");
        app.running = true;
        let cancellation = grokforge_core::TurnCancellation::new();
        app.turn_cancellation = Some(cancellation.clone());
        app.turn_handle = Some(tokio::spawn(async move {
            // Simulates an already-running host mutation, which must not be detached/aborted.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            TurnOutcome {
                session,
                rollout: None,
                panic: None,
            }
        }));
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.shutdown, super::ShutdownState::Requested);
        assert!(cancellation.is_cancelled());
        let result = super::wait_for_turn(&mut app.turn_handle).await;
        app.turn_complete_seen = true;
        app.finish_turn(result);
        assert_eq!(app.shutdown, super::ShutdownState::Ready);
    }

    #[tokio::test]
    async fn approval_arriving_after_shutdown_is_aborted_without_blocking_the_turn() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        app.request_quit();
        let (respond, wait) = oneshot::channel();
        app.on_approval_request(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/late".into(),
                },
                reason: "late request".to_string(),
            },
            respond,
        });

        assert_eq!(wait.await.expect("decision"), Decision::Abort);
        assert!(app.pending.is_none());
    }
}
