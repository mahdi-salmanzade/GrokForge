//! The interactive TUI application: an async event loop over terminal input, agent events, and
//! approval requests, rendering a scrolling transcript, a composer, a status line, and an
//! approval modal.
//!
//! This is the first working cut. It uses the alternate screen for robustness; the inline
//! viewport + native-scrollback render pipeline (the inline-scrollback differentiator in the design
//! docs) is the planned upgrade and slots in behind the same event/op flow.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::{FutureExt, StreamExt};
use grokforge_core::commands::{self, CommandDoc};
use grokforge_core::skills::{self, SkillDoc};
use grokforge_core::{Agent, RolloutWriter, Session, SessionMeta, TurnCancellation, sessions_dir};
use grokforge_protocol::{Decision, DenialClass, EventMsg, ResponseItem, Usage};
use grokforge_render::{LineKind, RenderLine, RenderSpan, SpanRole, render_markdown};
use grokforge_xai::{Effort, ServerTool, model_supports_effort};
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
const MAX_PALETTE_ROWS: usize = 7;
const REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// Upper bound on tracked subagent lanes (the core caps a turn at 32 spawns; this defends the TUI
/// against a misbehaving core emitting more).
const MAX_AGENT_LANES: usize = 64;
/// Most lanes rendered in the parallel-agents panel before collapsing the rest into a "+N more".
const MAX_AGENT_PANEL_ROWS: u16 = 12;
/// Path suggestions shown in the `@`-attach picker.
const AT_PICKER_LIMIT: usize = 8;
/// Prompts retained for Up/Down recall.
const MAX_INPUT_HISTORY: usize = 200;
/// Mouse wheels commonly emit several rapid events per gesture; three transcript rows per event
/// feels responsive without making short messages disappear in one notch.
const MOUSE_SCROLL_ROWS: u16 = 3;

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

/// One parallel-subagent lane shown in the "PARALLEL AGENTS" panel. Built from `SubagentStarted`
/// and updated by `SubagentUpdate`/`SubagentFinished`.
#[derive(Debug)]
struct AgentLane {
    id: String,
    label: String,
    /// Zero-based position within its spawn batch (displayed 1-based).
    index: usize,
    total: usize,
    activity: String,
    tokens: u64,
    status: LaneStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneStatus {
    Running,
    Done { ok: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownState {
    Active,
    Requested,
    Ready,
}

/// One locally discoverable slash action. The palette is derived from these records on every
/// composer edit; with a hard cap of 64 project commands this stays tiny and, importantly, never
/// sends project-command text to the model merely because the user typed `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashPaletteItem {
    completion: String,
    description: String,
    accepts_arguments: bool,
    requires_argument: bool,
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
    /// A pre-turn safety notice must remain visible without counting as conversation content.
    /// Otherwise a dirty-workspace warning suppresses the launch artwork before the first draw.
    startup_notice: Option<String>,
    streaming: Option<String>,
    reasoning: Option<String>,
    composer: String,
    /// `None` means the user dismissed the current slash deck with Esc. Any composer edit
    /// restores selection zero without adding another boolean to the app state machine.
    slash_selection: Option<usize>,
    scroll: u16,
    follow: bool,
    pending: Option<PendingApproval>,
    /// Approval requests can arrive concurrently from parallel subagents. Only the front request
    /// owns the modal; later requests wait here in arrival order until the user resolves it.
    approval_queue: VecDeque<PendingApproval>,
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
    /// Live parallel-subagent lanes for the current turn (cleared when a new turn starts).
    agents: Vec<AgentLane>,
    /// Monotonic redraw tick used to animate the agent spinners.
    frame: u64,
    /// Workspace root, retained so the `@`-attach picker works even while a turn holds the session.
    workspace_root: std::path::PathBuf,
    /// Selected row in the `@`-attach picker (`None` when the picker is dismissed/inactive).
    at_selection: Option<usize>,
    /// Cached path suggestions for the current `@`-mention query.
    at_matches: Vec<String>,
    /// Previously submitted prompts, recalled with Up/Down.
    input_history: Vec<String>,
    /// Cursor into `input_history` while recalling (`None` = editing the live draft).
    history_cursor: Option<usize>,
    /// The in-progress draft saved on the first Up, restored on Down past the newest entry.
    history_draft: String,

    // The channel stays open for the app's lifetime because `agent` (Arc) holds the sender.
    events_rx: mpsc::UnboundedReceiver<EventMsg>,
    approvals_rx: mpsc::UnboundedReceiver<PendingApproval>,
    turn_handle: Option<JoinHandle<TurnOutcome>>,
    turn_cancellation: Option<TurnCancellation>,
    turn_complete_seen: bool,
    undo_handle: Option<JoinHandle<Entry>>,
    model_handle: Option<JoinHandle<ModelSaveOutcome>>,
    effort_handle: Option<JoinHandle<EffortSaveOutcome>>,
}

#[derive(Debug)]
struct TurnOutcome {
    session: Session,
    rollout: Option<RolloutWriter>,
    panic: Option<String>,
}

#[derive(Debug)]
struct ModelSaveOutcome {
    model: String,
    context_window: Option<u64>,
    result: std::io::Result<()>,
}

#[derive(Debug)]
struct EffortSaveOutcome {
    effort: Option<Effort>,
    label: String,
    result: std::io::Result<()>,
}

/// Normal execute settings temporarily replaced for a single interactive plan turn.
#[derive(Debug)]
struct PlanConfigRestore {
    model: String,
    context_window_tokens: Option<u64>,
    effort: Option<Effort>,
}

impl PlanConfigRestore {
    fn apply(session: &mut Session) -> Result<Self, String> {
        let requested = session.config.plan_model.as_str();
        let selected = session.config.model_catalog.iter().find(|candidate| {
            candidate.id == requested || candidate.aliases.iter().any(|alias| alias == requested)
        });
        let (plan_model, context_window_tokens) = match selected {
            Some(model) => (model.id.clone(), model.context_window),
            None if session.config.model_catalog.is_empty() => (requested.to_string(), None),
            None => {
                return Err(format!(
                    "configured plan model `{requested}` is not advertised by the endpoint"
                ));
            }
        };
        let restore = Self {
            model: std::mem::replace(&mut session.config.model, plan_model),
            context_window_tokens: std::mem::replace(
                &mut session.config.context_window_tokens,
                context_window_tokens,
            ),
            effort: session.config.effort.replace(Effort::High),
        };
        Ok(restore)
    }

    fn restore(self, session: &mut Session) {
        session.config.model = self.model;
        session.config.context_window_tokens = self.context_window_tokens;
        session.config.effort = self.effort;
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("transcript_len", &self.transcript.len())
            .field("running", &self.running)
            .field("has_pending_approval", &self.pending.is_some())
            .field("queued_approvals", &self.approval_queue.len())
            .field("has_pending_undo", &self.undo_handle.is_some())
            .field("has_pending_model_save", &self.model_handle.is_some())
            .field("has_pending_effort_save", &self.effort_handle.is_some())
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
        let workspace_root = session.config.workspace_root.clone();
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
            startup_notice: None,
            streaming: None,
            reasoning: None,
            composer: String::new(),
            slash_selection: Some(0),
            scroll: 0,
            follow: true,
            pending: None,
            approval_queue: VecDeque::new(),
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
            agents: Vec::new(),
            frame: 0,
            workspace_root,
            at_selection: None,
            at_matches: Vec::new(),
            input_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            events_rx,
            approvals_rx,
            turn_handle: None,
            turn_cancellation: None,
            turn_complete_seen: false,
            undo_handle: None,
            model_handle: None,
            effort_handle: None,
        }
    }

    pub(crate) fn set_startup_notice(&mut self, message: impl Into<String>) {
        self.startup_notice = Some(bounded_text(&message.into(), MAX_ENTRY_BYTES));
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
        self.abort_all_approvals();
        self.finish_quit_if_quiescent();
    }

    fn finish_quit_if_quiescent(&mut self) {
        if self.shutdown == ShutdownState::Requested
            && self.turn_handle.is_none()
            && self.undo_handle.is_none()
            && self.model_handle.is_none()
            && self.effort_handle.is_none()
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
        if let Some(handle) = self.model_handle.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.effort_handle.take() {
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
                    // Animate the working braille wheel while a turn is in flight, and the
                    // parallel-agents spinners while any lane is still running.
                    if self.running || self.has_running_agents() {
                        self.frame = self.frame.wrapping_add(1);
                        redraw_needed = true;
                    }
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
                        Some(Ok(Event::Mouse(mouse))) => self.on_mouse(mouse),
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
                result = wait_for_model_save(&mut self.model_handle), if self.model_handle.is_some() => {
                    self.finish_model_save(result);
                    redraw_needed = true;
                }
                result = wait_for_effort_save(&mut self.effort_handle), if self.effort_handle.is_some() => {
                    self.finish_effort_save(result);
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

    /// Scroll the transcript with the mouse wheel. Mouse capture keeps the wheel from reaching the
    /// terminal (which would otherwise expose the native pre-launch scrollback behind the
    /// alternate screen); here it drives GrokForge's own transcript scrolling instead.
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                // The welcome deck has its own responsive layout rather than a scrollable
                // transcript. Do not leave follow mode or show a meaningless scroll badge there.
                if self.transcript.is_empty()
                    && self.streaming.is_none()
                    && self.reasoning.is_none()
                {
                    self.follow = true;
                    self.scroll = 0;
                    return;
                }
                self.scroll = if self.follow {
                    MOUSE_SCROLL_ROWS
                } else {
                    self.scroll.saturating_add(MOUSE_SCROLL_ROWS)
                };
                self.follow = false;
            }
            MouseEventKind::ScrollDown => {
                if self.follow {
                    // Maintain the follow-state invariant even if a caller constructed an
                    // inconsistent state (follow=true with a stale non-zero offset).
                    self.scroll = 0;
                    return;
                }
                self.scroll = self.scroll.saturating_sub(MOUSE_SCROLL_ROWS);
                if self.scroll == 0 {
                    self.follow = true;
                }
            }
            // Capture is enabled for wheel delivery only. Click, drag, movement, and horizontal
            // wheel events intentionally have no application meaning and cannot mutate state.
            _ => {}
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

        // Match the shell convention without stealing Ctrl+D from approval handling or from an
        // in-flight turn. A non-empty draft is deliberately preserved.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('d')
            && self.composer.is_empty()
            && !self.running
        {
            self.request_quit();
            return;
        }

        if self.on_slash_palette_key(key) {
            return;
        }
        if self.on_at_palette_key(key) {
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
                ) && is_safe_terminal_char(c) =>
            {
                if self.composer.len().saturating_add(c.len_utf8()) <= MAX_COMPOSER_BYTES {
                    self.composer.push(c);
                    self.on_composer_changed();
                }
            }
            KeyCode::Backspace => {
                if self.composer.pop().is_some() {
                    self.on_composer_changed();
                }
            }
            // Shell-style: the arrows recall previously entered prompts; the transcript scrolls
            // with PageUp/PageDown (and End jumps to the latest).
            KeyCode::Up => self.history_recall_prev(),
            KeyCode::Down => self.history_recall_next(),
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

    /// Handles keys only while the slash deck is visible. Approval input is dispatched before
    /// this method, preserving Esc-as-deny and approval paging semantics.
    fn on_slash_palette_key(&mut self, key: KeyEvent) -> bool {
        let Some(selection) = self.slash_selection else {
            return false;
        };
        if !self.composer.starts_with('/') {
            return false;
        }
        let matches = self.slash_palette_matches();
        match key.code {
            KeyCode::Esc => {
                self.slash_selection = None;
                true
            }
            KeyCode::Up if !matches.is_empty() => {
                self.slash_selection = Some(if selection == 0 {
                    matches.len() - 1
                } else {
                    selection.min(matches.len() - 1) - 1
                });
                true
            }
            KeyCode::Down if !matches.is_empty() => {
                self.slash_selection = Some((selection + 1) % matches.len());
                true
            }
            KeyCode::Tab if !matches.is_empty() => {
                let selected = matches[selection.min(matches.len() - 1)].clone();
                self.complete_slash_item(&selected);
                true
            }
            KeyCode::Enter if !matches.is_empty() => {
                let selected = matches[selection.min(matches.len() - 1)].clone();
                self.composer.clone_from(&selected.completion);
                if selected.requires_argument {
                    self.composer.push(' ');
                    self.on_composer_changed();
                } else {
                    self.submit();
                }
                true
            }
            _ => false,
        }
    }

    fn complete_slash_item(&mut self, item: &SlashPaletteItem) {
        self.composer.clone_from(&item.completion);
        if item.accepts_arguments {
            self.composer.push(' ');
        }
        self.on_composer_changed();
    }

    fn on_composer_changed(&mut self) {
        self.slash_selection = Some(0);
        self.refresh_at_state();
    }

    /// The active `@`-mention query (the text after a trailing `@word`), or `None` when the
    /// composer is not currently typing a mention. Slash commands take precedence.
    fn at_query(&self) -> Option<String> {
        if self.composer.starts_with('/') || self.composer.ends_with(char::is_whitespace) {
            return None;
        }
        let last = self.composer.split_whitespace().next_back()?;
        let rest = last.strip_prefix('@')?;
        if rest.contains('@') {
            return None;
        }
        Some(rest.to_string())
    }

    /// Recompute the `@`-attach suggestions for the current composer state.
    fn refresh_at_state(&mut self) {
        if let Some(query) = self.at_query() {
            self.at_matches =
                grokforge_core::attach::search_paths(&self.workspace_root, &query, AT_PICKER_LIMIT);
            self.at_selection = if self.at_matches.is_empty() {
                None
            } else {
                Some(0)
            };
        } else {
            self.at_matches.clear();
            self.at_selection = None;
        }
    }

    /// Handle keys while the `@`-attach picker is visible. Returns whether the key was consumed.
    fn on_at_palette_key(&mut self, key: KeyEvent) -> bool {
        let Some(selection) = self.at_selection else {
            return false;
        };
        if self.at_matches.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.at_selection = None;
                true
            }
            KeyCode::Up => {
                let len = self.at_matches.len();
                self.at_selection = Some(if selection == 0 {
                    len - 1
                } else {
                    selection - 1
                });
                true
            }
            KeyCode::Down => {
                self.at_selection = Some((selection + 1) % self.at_matches.len());
                true
            }
            KeyCode::Tab | KeyCode::Enter => {
                let path = self.at_matches[selection.min(self.at_matches.len() - 1)].clone();
                self.complete_at_item(&path);
                true
            }
            _ => false,
        }
    }

    /// Replace the trailing `@query` with the chosen path and commit it with a trailing space,
    /// which closes the picker so the next Enter submits.
    fn complete_at_item(&mut self, path: &str) {
        if let Some(at) = self.composer.rfind('@') {
            self.composer.truncate(at);
        }
        let addition = format_at_mention(path);
        if self.composer.len().saturating_add(addition.len()) <= MAX_COMPOSER_BYTES {
            self.composer.push_str(&addition);
        }
        self.on_composer_changed();
    }

    fn on_paste(&mut self, value: &str) {
        if self.pending.is_some() {
            return;
        }
        let value = safe_terminal_text(value);
        let remaining = MAX_COMPOSER_BYTES.saturating_sub(self.composer.len());
        let mut used = 0usize;
        self.composer.extend(value.chars().take_while(|ch| {
            used = used.saturating_add(ch.len_utf8());
            used <= remaining
        }));
        self.on_composer_changed();
    }

    fn on_approval_request(&mut self, pending: PendingApproval) {
        // Ctrl+C can race with an approval being emitted. Never leave the turn blocked on a
        // request that arrived after shutdown started.
        if self.shutdown != ShutdownState::Active {
            let _ = pending.respond.send(Decision::Abort);
            return;
        }
        self.approval_queue.push_back(pending);
        self.activate_next_approval();
    }

    /// Promote the oldest queued request into the modal. Keeping this transition synchronous with
    /// the previous decision means there is never a frame where a later request can overwrite an
    /// unresolved one or accept composer input between consecutive approvals.
    fn activate_next_approval(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(pending) = self.approval_queue.pop_front() else {
            self.approval_detail = None;
            return;
        };
        self.approval_detail = Some(ApprovalDetail::new(safe_terminal_text(&format!(
            "reason: {}\nrequest: {:?}",
            pending.request.reason, pending.request.kind
        ))));
        self.pending = Some(pending);
    }

    /// Fail every outstanding request closed when the UI starts shutting down. Draining the queue
    /// is essential for parallel subagents: otherwise their approval futures remain blocked after
    /// the visible request is aborted.
    fn abort_all_approvals(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.respond.send(Decision::Abort);
        }
        while let Some(pending) = self.approval_queue.pop_front() {
            let _ = pending.respond.send(Decision::Abort);
        }
        self.approval_detail = None;
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
            self.activate_next_approval();
        }
    }

    fn submit(&mut self) {
        // The composer is sanitized on every input path, and again here as a final boundary before
        // text can reach project-command expansion, persistence, or the model request ledger.
        let text = safe_terminal_text(&self.composer).trim().to_string();
        if text.is_empty() || self.running {
            return;
        }
        self.record_history(&text);
        self.composer.clear();
        self.slash_selection = Some(0);
        if text.eq_ignore_ascii_case("exit") || text.eq_ignore_ascii_case("quit") {
            self.request_quit();
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.handle_slash(cmd);
            return;
        }
        self.push_entry(Entry::User(text.clone()));
        self.follow = true;
        self.start_turn(text, false);
    }

    /// Record a submitted prompt in the input history (skipping consecutive duplicates) and reset
    /// the recall cursor so the next Up starts from the newest entry.
    fn record_history(&mut self, text: &str) {
        if self.input_history.last().map(String::as_str) != Some(text) {
            self.input_history.push(text.to_string());
            if self.input_history.len() > MAX_INPUT_HISTORY {
                self.input_history.remove(0);
            }
        }
        self.history_cursor = None;
        self.history_draft.clear();
    }

    /// Up: recall an older prompt. The first Up saves the current draft so Down can restore it.
    fn history_recall_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            None => {
                self.history_draft.clone_from(&self.composer);
                self.input_history.len() - 1
            }
            Some(0) => 0,
            Some(current) => current - 1,
        };
        self.history_cursor = Some(index);
        self.composer.clone_from(&self.input_history[index]);
        self.on_composer_changed();
    }

    /// Down: move toward newer prompts; past the newest, restore the draft that was being typed.
    fn history_recall_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.input_history.len() {
            self.history_cursor = Some(index + 1);
            self.composer.clone_from(&self.input_history[index + 1]);
        } else {
            self.history_cursor = None;
            self.composer = std::mem::take(&mut self.history_draft);
        }
        self.on_composer_changed();
    }

    /// Spawn a turn (execute or plan mode), moving the session + rollout into the task.
    fn start_turn(&mut self, text: String, plan: bool) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let plan_restore = if plan {
            match PlanConfigRestore::apply(&mut session) {
                Ok(restore) => Some(restore),
                Err(message) => {
                    self.session = Some(session);
                    self.push_entry(Entry::Error(message));
                    return;
                }
            }
        } else {
            None
        };
        let mut rollout = self.rollout.take();
        self.running = true;
        self.turn_complete_seen = false;
        self.reasoning = None;
        self.active_tool = None;
        self.stream_retry = None;
        self.agents.clear();
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
            if let Some(restore) = plan_restore {
                restore.restore(&mut session);
            }
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
                    "commands: /plan <task>  ·  /model [slug]  ·  /effort [auto|low|medium|high|xhigh]  ·  /skills [name]  ·  /memory  ·  /tools [web|x|code] [on|off]  ·  /undo  ·  /clear  ·  /quit".to_string(),
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
            "memory" => self.show_memory(),
            "model" => self.handle_model(rest.trim()),
            "effort" => self.handle_effort(rest.trim()),
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

    fn slash_palette_items(&self) -> Vec<SlashPaletteItem> {
        let builtin = [
            ("/help", "Command map and keyboard shortcuts", false, false),
            (
                "/plan",
                "Design a solution without changing files",
                true,
                true,
            ),
            ("/skills", "Browse local project skills", true, false),
            (
                "/model",
                "List available models or switch the active model",
                true,
                false,
            ),
            ("/effort", "Show or set reasoning effort", true, false),
            (
                "/memory",
                "Show persistent memory (.grokforge/memory/)",
                false,
                false,
            ),
            (
                "/tools",
                "Open the capability deck: local tools + hosted toggles",
                true,
                false,
            ),
            (
                "/tools web on",
                "Enable hosted web search · separately metered",
                false,
                false,
            ),
            ("/tools web off", "Disable hosted web search", false, false),
            (
                "/tools x on",
                "Enable hosted X search · separately metered",
                false,
                false,
            ),
            ("/tools x off", "Disable hosted X search", false, false),
            (
                "/tools code on",
                "Enable hosted code interpreter · separately metered",
                false,
                false,
            ),
            (
                "/tools code off",
                "Disable hosted code interpreter",
                false,
                false,
            ),
            (
                "/undo",
                "Undo an isolated-worktree agent commit · foreground journal pending",
                false,
                false,
            ),
            ("/clear", "Clear the visible transcript", false, false),
            ("/quit", "Close GrokForge", false, false),
        ];
        let mut items = builtin
            .into_iter()
            .map(
                |(completion, description, accepts_arguments, requires_argument)| {
                    SlashPaletteItem {
                        completion: completion.to_string(),
                        description: description.to_string(),
                        accepts_arguments,
                        requires_argument,
                    }
                },
            )
            .collect::<Vec<_>>();
        items.extend(
            self.project_commands
                .iter()
                .map(|command| SlashPaletteItem {
                    completion: format!("/{}", command.name),
                    description: project_command_description(command),
                    accepts_arguments: true,
                    requires_argument: false,
                }),
        );
        items
    }

    fn slash_palette_matches(&self) -> Vec<SlashPaletteItem> {
        let Some(query) = self.composer.strip_prefix('/') else {
            return Vec::new();
        };
        let query = safe_terminal_line(query).to_ascii_lowercase();
        self.slash_palette_items()
            .into_iter()
            .filter(|item| {
                item.completion[1..]
                    .to_ascii_lowercase()
                    .starts_with(&query)
            })
            .collect()
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

    fn handle_model(&mut self, requested: &str) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if requested.is_empty() {
            let current = session.config.model.clone();
            let available = session
                .config
                .model_catalog
                .iter()
                .map(|model| model.id.as_str())
                .take(32)
                .collect::<Vec<_>>()
                .join("  ·  ");
            self.push_entry(Entry::Info(format!("active model · {current}")));
            if available.is_empty() {
                self.push_entry(Entry::Info(
                    "model catalog unavailable · restart with --model <slug> to validate a different model"
                        .to_string(),
                ));
            } else {
                self.push_entry(Entry::Info(format!(
                    "available models · {available} · switch with /model <slug>"
                )));
            }
            return;
        }
        if requested.len() > 160
            || requested.trim() != requested
            || requested.chars().any(char::is_control)
            || requested.chars().any(char::is_whitespace)
        {
            self.push_entry(Entry::Info(
                "usage: /model <advertised-model-slug>".to_string(),
            ));
            return;
        }
        let selected = session
            .config
            .model_catalog
            .iter()
            .find(|model| {
                model.id == requested || model.aliases.iter().any(|alias| alias == requested)
            })
            .cloned();
        let Some(selected) = selected else {
            self.push_entry(Entry::Info(format!(
                "model `{requested}` was not in the startup catalog · run /model to see available models"
            )));
            return;
        };
        if session
            .config
            .effort
            .is_some_and(|effort| !model_supports_effort(&selected.id, effort))
        {
            self.push_entry(Entry::Info(
                "model switch blocked · xhigh requires an xAI multi-agent model · set /effort auto or /effort high first"
                    .to_string(),
            ));
            return;
        }
        if selected.id == session.config.model {
            self.push_entry(Entry::Info(format!(
                "model already active · {}",
                selected.id
            )));
            return;
        }
        let Ok(dir) = sessions_dir() else {
            self.push_entry(Entry::Error(
                "cannot persist model switch: session directory unavailable".to_string(),
            ));
            return;
        };
        let session_id = session.id;
        let model = selected.id;
        let context_window = selected.context_window;
        self.running = true;
        self.push_entry(Entry::Info(format!("switching model · {model}")));
        self.model_handle = Some(tokio::spawn(async move {
            let result = SessionMeta::update_model(&dir, session_id, model.clone()).await;
            ModelSaveOutcome {
                model,
                context_window,
                result,
            }
        }));
    }

    fn handle_effort(&mut self, requested: &str) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if requested.is_empty() {
            let current = match session.config.effort {
                None => "auto",
                Some(Effort::Low) => "low",
                Some(Effort::Medium) => "medium",
                Some(Effort::High) => "high",
                Some(Effort::Xhigh) => "xhigh",
            };
            self.push_entry(Entry::Info(format!(
                "reasoning effort · {current} · set with /effort <auto|low|medium|high|xhigh>"
            )));
            return;
        }
        let effort = match requested {
            "auto" => None,
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            _ => {
                self.push_entry(Entry::Info(
                    "usage: /effort <auto|low|medium|high|xhigh>".to_string(),
                ));
                return;
            }
        };
        if effort.is_some_and(|effort| !model_supports_effort(&session.config.model, effort)) {
            self.push_entry(Entry::Info(
                "reasoning effort xhigh requires an xAI multi-agent model".to_string(),
            ));
            return;
        }
        if effort == session.config.effort {
            self.push_entry(Entry::Info(format!(
                "reasoning effort already active · {requested}"
            )));
            return;
        }
        let Ok(dir) = sessions_dir() else {
            self.push_entry(Entry::Error(
                "cannot persist effort switch: session directory unavailable".to_string(),
            ));
            return;
        };
        let session_id = session.id;
        let label = requested.to_string();
        self.running = true;
        self.push_entry(Entry::Info(format!(
            "saving reasoning effort · {requested}"
        )));
        self.effort_handle = Some(tokio::spawn(async move {
            let result = SessionMeta::update_effort(&dir, session_id, effort).await;
            EffortSaveOutcome {
                effort,
                label,
                result,
            }
        }));
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
            self.push_entry(Entry::Info(
                "LOCAL CAPABILITIES · available without hosted-tool charges".to_string(),
            ));
            self.push_entry(Entry::Info(
                "read · write · edit · list · glob · grep · shell".to_string(),
            ));
            self.push_entry(Entry::Info(
                "git status · git diff · spawn task".to_string(),
            ));
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

    /// Show the agent's persistent memory (the auto-loaded `.grokforge/memory/MEMORY.md` index).
    fn show_memory(&mut self) {
        let Some(session) = self.session.as_ref() else {
            self.push_entry(Entry::Info(
                "memory is available between turns; try again when idle.".to_string(),
            ));
            return;
        };
        let docs = grokforge_core::memory::discover(&session.config.workspace_root);
        if docs.is_empty() {
            self.push_entry(Entry::Info(
                "no memory yet · Grok saves durable notes to .grokforge/memory/ with the `remember` tool".to_string(),
            ));
            return;
        }
        self.push_entry(Entry::Info(
            "MEMORY · .grokforge/memory/MEMORY.md".to_string(),
        ));
        for doc in docs {
            let body = bounded_text(&safe_terminal_text(&doc.content), 8 * 1024);
            self.push_entry(Entry::Info(body));
        }
    }

    /// Undo the last agent commit for this session (git, from the host process).
    fn undo(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if !session.config.isolated_worktree {
            self.push_entry(Entry::Info(
                "foreground undo is not available yet · GrokForge leaves shared-worktree edits uncommitted to avoid racing your editor; review with git diff"
                    .to_string(),
            ));
            return;
        }
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
            EventMsg::SubagentStarted {
                agent_id,
                label,
                index,
                total,
            } => self.upsert_agent_lane(agent_id, &label, index, total),
            EventMsg::SubagentUpdate { agent_id, inner } => {
                self.on_subagent_update(&agent_id, &inner);
            }
            EventMsg::SubagentFinished {
                agent_id,
                ok,
                summary,
            } => {
                if let Some(lane) = self.agent_lane_mut(&agent_id) {
                    lane.status = LaneStatus::Done { ok };
                    lane.activity = bounded_text(&safe_terminal_line(&summary), 100);
                }
            }
            EventMsg::SessionConfigured { .. }
            | EventMsg::TurnStarted { .. }
            | EventMsg::ToolOutputDelta { .. }
            | EventMsg::ApprovalRequested(_)
            | EventMsg::ShutdownComplete => {}
        }
    }

    fn has_running_agents(&self) -> bool {
        self.agents
            .iter()
            .any(|lane| matches!(lane.status, LaneStatus::Running))
    }

    fn agent_lane_mut(&mut self, id: &str) -> Option<&mut AgentLane> {
        self.agents.iter_mut().find(|lane| lane.id == id)
    }

    fn upsert_agent_lane(&mut self, id: String, label: &str, index: usize, total: usize) {
        let label = bounded_text(&safe_terminal_line(label), 64);
        if let Some(lane) = self.agent_lane_mut(&id) {
            lane.label = label;
            lane.index = index;
            lane.total = total;
            lane.status = LaneStatus::Running;
            lane.activity = "starting…".to_string();
        } else if self.agents.len() < MAX_AGENT_LANES {
            self.agents.push(AgentLane {
                id,
                label,
                index,
                total,
                activity: "starting…".to_string(),
                tokens: 0,
                status: LaneStatus::Running,
            });
        }
    }

    /// Apply a lane-tagged subagent event: fold its accounting into the global totals (so cost and
    /// privacy stay correct despite the per-lane attribution) and update the lane's live activity.
    fn on_subagent_update(&mut self, agent_id: &str, inner: &EventMsg) {
        match inner {
            EventMsg::TokenUsage { usage } => {
                self.usage.add(*usage);
                let delta = usage.input_tokens.saturating_add(usage.output_tokens);
                if let Some(lane) = self.agent_lane_mut(agent_id) {
                    lane.tokens = lane.tokens.saturating_add(delta);
                }
            }
            EventMsg::LedgerAppended(entry) => {
                self.ledger_sources = self.ledger_sources.saturating_add(1);
                self.ledger_bytes = self.ledger_bytes.saturating_add(entry.bytes);
                self.ledger_redactions = self.ledger_redactions.saturating_add(entry.redactions);
            }
            _ => {}
        }
        if let Some(activity) = subagent_activity(inner)
            && let Some(lane) = self.agent_lane_mut(agent_id)
        {
            lane.activity = activity;
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

    fn finish_model_save(&mut self, result: Result<ModelSaveOutcome, tokio::task::JoinError>) {
        self.model_handle.take();
        self.running = false;
        match result {
            Ok(ModelSaveOutcome {
                model,
                context_window,
                result: Ok(()),
            }) => {
                if let Some(session) = self.session.as_mut() {
                    session.config.model.clone_from(&model);
                    session.config.context_window_tokens = context_window;
                }
                self.status_model.clone_from(&model);
                self.push_entry(Entry::Info(format!(
                    "model active · {model} · saved for resume"
                )));
            }
            Ok(ModelSaveOutcome {
                result: Err(error), ..
            }) => self.push_entry(Entry::Error(format!(
                "model switch was not applied because session metadata could not be saved: {error}"
            ))),
            Err(error) => {
                self.push_entry(Entry::Error(format!("model switch task failed: {error}")));
            }
        }
        self.finish_quit_if_quiescent();
    }

    fn finish_effort_save(&mut self, result: Result<EffortSaveOutcome, tokio::task::JoinError>) {
        self.effort_handle.take();
        self.running = false;
        match result {
            Ok(EffortSaveOutcome {
                effort,
                label,
                result: Ok(()),
            }) => {
                if let Some(session) = self.session.as_mut() {
                    session.config.effort = effort;
                }
                self.push_entry(Entry::Info(format!(
                    "reasoning effort active · {label} · saved for resume"
                )));
            }
            Ok(EffortSaveOutcome {
                result: Err(error), ..
            }) => self.push_entry(Entry::Error(format!(
                "effort switch was not applied because session metadata could not be saved: {error}"
            ))),
            Err(error) => {
                self.push_entry(Entry::Error(format!("effort switch task failed: {error}")));
            }
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
        // A one-line activity band sits directly above the composer (Claude Code style) while
        // GrokForge is working, so the live status is where the user is looking — not tucked into
        // the bottom-left status bar.
        let activity_height = u16::from(
            self.working_activity().is_some() && remaining.saturating_sub(composer_height) >= 2,
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(0),
                Constraint::Length(activity_height),
                Constraint::Length(composer_height),
                Constraint::Length(status_height),
            ])
            .split(area);

        self.render_header(chunks[0], f);
        let transcript_area = self.render_agents_panel(chunks[1], f);
        self.render_transcript(transcript_area, f);
        self.render_activity(chunks[2], f);
        self.render_composer(chunks[3], f);
        self.render_status(chunks[4], f);
        self.render_slash_palette(chunks[1], f);
        self.render_at_palette(chunks[1], f);

        if let (Some(pending), Some(detail)) = (&self.pending, &self.approval_detail) {
            render_approval_modal(area, f, pending, detail);
        }
        apply_display_fallback(f, area, self.display_mode);
    }

    /// Render the live parallel-agents panel at the top of `area` (if any lanes exist and there is
    /// room) and return the remaining area for the transcript. This is the "cool TUI" surface for
    /// up to 32 subagents running at once: one animated row per lane with its activity and tokens.
    fn render_agents_panel(&self, area: Rect, f: &mut ratatui::Frame) -> Rect {
        if self.agents.is_empty() || area.height < 7 || area.width < 24 {
            return area;
        }
        let running = self
            .agents
            .iter()
            .filter(|lane| matches!(lane.status, LaneStatus::Running))
            .count();
        let done = self.agents.len() - running;

        // Never let the panel eat more than roughly half the transcript region.
        let max_rows = (area.height.saturating_sub(4) / 2).clamp(1, MAX_AGENT_PANEL_ROWS);
        let shown = u16::try_from(self.agents.len())
            .unwrap_or(u16::MAX)
            .min(max_rows);
        let overflow = u16::try_from(self.agents.len())
            .unwrap_or(u16::MAX)
            .saturating_sub(shown);
        let inner_rows = shown + u16::from(overflow > 0);
        let panel_height = inner_rows + 2;
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(panel_height), Constraint::Min(0)])
            .split(area);
        let panel = split[0];

        let ascii = self.display_mode.ascii;
        let bolt = if ascii { ">>" } else { "⚡" };
        let title = format!(" {bolt} PARALLEL AGENTS · {running} running · {done} done ");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                title,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(SURFACE_RAISED).fg(TEXT));
        let inner = block.inner(panel);
        f.render_widget(block, panel);

        let mut lines: Vec<Line<'static>> = self
            .agents
            .iter()
            .take(shown as usize)
            .map(|lane| self.agent_lane_line(lane, inner.width))
            .collect();
        if overflow > 0 {
            lines.push(Line::from(Span::styled(
                format!("  … +{overflow} more agent(s)"),
                Style::default().fg(MUTED),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
        split[1]
    }

    fn agent_lane_line(&self, lane: &AgentLane, width: u16) -> Line<'static> {
        let (glyph, color) = match lane.status {
            LaneStatus::Running => (self.spinner_glyph(lane.index).to_string(), ACCENT_SOFT),
            LaneStatus::Done { ok: true } => (
                (if self.display_mode.ascii { "+" } else { "✓" }).to_string(),
                SUCCESS,
            ),
            LaneStatus::Done { ok: false } => (
                (if self.display_mode.ascii { "x" } else { "✗" }).to_string(),
                DANGER,
            ),
        };
        let pos = format!("{:>2}/{:<2}", lane.index + 1, lane.total);
        let tokens = compact_tokens(lane.tokens);
        let width = width as usize;
        // Keep both useful columns visible even in the 22-cell interior of a 24-column panel.
        // Everything outside label/activity is fixed chrome: glyph + spaces + position + tokens.
        let fixed_width = display_width(&glyph)
            .saturating_add(1)
            .saturating_add(display_width(&pos))
            .saturating_add(1)
            .saturating_add(1)
            .saturating_add(1)
            .saturating_add(display_width(&tokens));
        let flexible_width = width.saturating_sub(fixed_width);
        let label_width = flexible_width.saturating_sub(4).min(22);
        let activity_width = flexible_width.saturating_sub(label_width);
        let label = pad_or_truncate(&lane.label, label_width);
        let activity = pad_or_truncate(&lane.activity, activity_width);
        Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(format!("{pos} "), Style::default().fg(MUTED)),
            Span::styled(
                format!("{label} "),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{activity} "), Style::default().fg(MUTED)),
            Span::styled(tokens, Style::default().fg(FAINT)),
        ])
    }

    fn spinner_glyph(&self, offset: usize) -> &'static str {
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frames: &[&str] = if self.display_mode.ascii {
            &ASCII
        } else {
            &BRAILLE
        };
        let phase = usize::try_from(self.frame % frames.len() as u64).unwrap_or(0);
        frames[phase.wrapping_add(offset) % frames.len()]
    }

    fn activity_state(&self) -> (String, Color) {
        // Animated braille wheel while GrokForge is actually working (tool / reasoning / turn).
        let spin = self.working_spinner();
        if self.pending.is_some() {
            ("● APPROVAL".to_string(), WARNING)
        } else if let Some(attempt) = self.stream_retry {
            (format!("↻ RETRY {attempt}"), WARNING)
        } else if let Some(tool) = &self.active_tool {
            (format!("{spin} {}", compact_preview(tool, 18)), TOOL)
        } else if self.reasoning.is_some() {
            (format!("{spin} TRACING"), MUTED)
        } else if self.running {
            (format!("{spin} WORKING"), ACCENT)
        } else {
            ("● READY".to_string(), SUCCESS)
        }
    }

    /// Live activity to show in the band above the composer (Claude Code style), or `None` when
    /// idle (nothing is shown, the composer just waits for input).
    fn working_activity(&self) -> Option<(String, Color)> {
        let spin = self.working_spinner();
        if self.pending.is_some() {
            Some(("● approval needed — respond above".to_string(), WARNING))
        } else if let Some(attempt) = self.stream_retry {
            Some((format!("↻ retrying (attempt {attempt})…"), WARNING))
        } else if let Some(tool) = &self.active_tool {
            Some((format!("{spin} {}…", compact_preview(tool, 24)), TOOL))
        } else if self.reasoning.is_some() {
            Some((format!("{spin} thinking…"), MUTED))
        } else if self.running {
            Some((format!("{spin} working…  ·  esc to interrupt"), ACCENT))
        } else {
            None
        }
    }

    /// Render the one-line activity band directly above the composer.
    fn render_activity(&self, area: Rect, f: &mut ratatui::Frame) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some((text, color)) = self.working_activity() else {
            return;
        };
        f.render_widget(
            Block::default().style(Style::default().bg(CANVAS).fg(TEXT)),
            area,
        );
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), horizontal_inset(area, 1));
    }

    /// The current frame of the "working" braille wheel (animated by the redraw tick while a turn
    /// is in flight). Falls back to an ASCII spinner where the palette can't render braille.
    fn working_spinner(&self) -> &'static str {
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        const WHEEL: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
        let frames: &[&str] = if self.display_mode.ascii {
            &ASCII
        } else {
            &WHEEL
        };
        let phase = usize::try_from(self.frame % frames.len() as u64).unwrap_or(0);
        frames[phase]
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
        let brand = if content.width >= 18 {
            Line::from(vec![
                Span::styled("@%#", Style::default().fg(ACCENT)),
                Span::styled("*+=  ", Style::default().fg(ACCENT_SOFT)),
                Span::styled(
                    "GROKFORGE",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ])
        } else if content.width >= 8 {
            Line::from(vec![
                Span::styled("@%  ", Style::default().fg(ACCENT_SOFT)),
                Span::styled("GF", Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            ])
        } else {
            Line::from(Span::styled(
                "GF",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ))
        };
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
                Span::styled("FORGE CORE", Style::default().fg(ACCENT_SOFT)),
                Span::styled("  //  ", Style::default().fg(FAINT)),
                Span::styled("TRACE · SHAPE · PROVE", Style::default().fg(MUTED)),
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
            if let Some(notice) = &self.startup_notice {
                render_startup_notice(content_area, notice, f);
            }
            return;
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(notice) = &self.startup_notice {
            push_startup_notice_lines(&mut lines, notice, content_width);
        }
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
                Span::styled("◇  TRACE  ", Style::default().fg(FAINT)),
                Span::styled(compact_preview(reasoning, 112), Style::default().fg(MUTED)),
                Span::styled("  ●", Style::default().fg(ACCENT)),
            ]));
        }
        if let Some(streaming) = &self.streaming {
            push_role_header(
                &mut lines,
                "◆",
                "GROK // FORGE",
                ACCENT,
                Some("● GENERATING"),
            );
            let streaming = safe_terminal_text(streaming);
            push_markdown_body(&mut lines, &streaming, content_width);
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

        // The live activity indicator now lives in the band above the composer; the bottom line
        // carries persistent metadata only, led by the model.
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                safe_terminal_line(&self.status_model),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ];
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
                Span::styled(" cast   ", Style::default().fg(MUTED)),
                Span::styled("↑↓", Style::default().fg(TEXT)),
                Span::styled(" scroll   ", Style::default().fg(MUTED)),
                Span::styled("/ deck", Style::default().fg(TEXT)),
                Span::styled(
                    if self.running {
                        "   Ctrl+C stop + exit  "
                    } else {
                        "   Ctrl+C quit  "
                    },
                    Style::default().fg(MUTED),
                ),
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
        let prefix = if compact { "@> " } else { " @>  " };
        let prefix_width = u16::try_from(Line::from(prefix).width()).unwrap_or(0);
        let border_width = u16::from(!compact);
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
                    "Describe the work · / opens tools + commands"
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
                .borders(Borders::LEFT | Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(border_color))
                .title(Line::from(Span::styled(
                    " CAST ",
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
            let max_x = area.right().saturating_sub(1);
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

    /// Render the `@`-attach path picker as a popup at the bottom of `area`, listing the current
    /// fuzzy suggestions with the selection highlighted.
    fn render_at_palette(&self, area: Rect, f: &mut ratatui::Frame) {
        if self.pending.is_some() || area.width < 12 || area.height < 3 {
            return;
        }
        let Some(selection) = self.at_selection else {
            return;
        };
        if self.at_matches.is_empty() || self.at_query().is_none() {
            return;
        }
        let selected = selection.min(self.at_matches.len() - 1);
        let inset = if area.width >= 40 { 4 } else { 0 };
        let width = area.width.saturating_sub(inset).clamp(1, 72);
        let rows = self
            .at_matches
            .len()
            .min(AT_PICKER_LIMIT)
            .min(usize::from(area.height.saturating_sub(2)).max(1));
        let height = u16::try_from(rows)
            .unwrap_or(1)
            .saturating_add(2)
            .min(area.height);
        let palette_area = Rect::new(
            area.x.saturating_add(inset),
            area.bottom().saturating_sub(height),
            width,
            height,
        );
        let start = selected
            .saturating_add(1)
            .saturating_sub(rows)
            .min(self.at_matches.len().saturating_sub(rows));
        let inner_width = usize::from(width.saturating_sub(2));
        let lines = self.at_matches[start..start + rows]
            .iter()
            .enumerate()
            .map(|(offset, path)| {
                let chosen = start + offset == selected;
                let marker = if chosen { "▸ " } else { "  " };
                let text = pad_or_truncate(path, inner_width.saturating_sub(2));
                Line::from(vec![
                    Span::styled(
                        marker.to_string(),
                        Style::default().fg(if chosen { ACCENT } else { FAINT }),
                    ),
                    Span::styled(text, Style::default().fg(if chosen { TEXT } else { MUTED })),
                ])
            })
            .collect::<Vec<_>>();

        f.render_widget(Clear, palette_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " @ ATTACH · ↑↓ · TAB/ENTER · ESC ",
                Style::default().fg(ACCENT),
            ))
            .style(Style::default().bg(SURFACE).fg(TEXT));
        let inner = block.inner(palette_area);
        f.render_widget(block, palette_area);
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_slash_palette(&self, area: Rect, f: &mut ratatui::Frame) {
        if self.pending.is_some()
            || !self.composer.starts_with('/')
            || area.width == 0
            || area.height == 0
        {
            return;
        }
        let Some(selection) = self.slash_selection else {
            return;
        };
        let items = self.slash_palette_matches();
        if items.is_empty() {
            return;
        }

        let selected = selection.min(items.len() - 1);
        let bordered = area.width >= 16 && area.height >= 3;
        let border_rows = if bordered { 2 } else { 0 };
        let inset = if area.width >= 40 { 4 } else { 0 };
        let width = area.width.saturating_sub(inset).clamp(1, 88);
        let available_rows = area.height.saturating_sub(border_rows);
        let show_toolbox = width >= 26 && available_rows >= 2;
        let toolbox_rows = u16::from(show_toolbox);
        let row_count = items
            .len()
            .min(MAX_PALETTE_ROWS)
            .min(usize::from(available_rows.saturating_sub(toolbox_rows)).max(1));
        let height = u16::try_from(row_count)
            .unwrap_or(area.height)
            .saturating_add(toolbox_rows)
            .saturating_add(border_rows)
            .min(area.height);
        let palette_area = Rect::new(
            area.x.saturating_add(inset),
            area.bottom().saturating_sub(height),
            width,
            height,
        );
        let start = selected
            .saturating_add(1)
            .saturating_sub(row_count)
            .min(items.len().saturating_sub(row_count));
        let visible = &items[start..start + row_count];
        let inner_width = usize::from(width.saturating_sub(if bordered { 2 } else { 0 }));
        let name_width = visible
            .iter()
            .map(|item| item.completion.len())
            .max()
            .unwrap_or(0)
            .min(inner_width.saturating_sub(4) / 2)
            .max(1);
        let show_descriptions = inner_width >= 34;
        let mut lines = visible
            .iter()
            .enumerate()
            .map(|(offset, item)| {
                slash_palette_line(
                    item,
                    start + offset == selected,
                    name_width,
                    show_descriptions,
                )
            })
            .collect::<Vec<_>>();
        if show_toolbox {
            lines.push(slash_toolbox_line(inner_width));
        }

        f.render_widget(Clear, palette_area);
        let mut block = Block::default().style(Style::default().bg(SURFACE).fg(TEXT));
        if bordered {
            let title = if width >= 76 {
                format!(
                    " / FORGE DECK · {} FOUND · ↑↓ PICK · TAB COMPLETE · ENTER SELECT · ESC CLOSE ",
                    items.len()
                )
            } else {
                format!(" / FORGE DECK · {} ", items.len())
            };
            block = block
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(ACCENT_SOFT)
                        .add_modifier(Modifier::BOLD),
                )));
        }
        f.render_widget(Paragraph::new(lines).block(block), palette_area);
    }
}

/// Encode a picker result in the quoted syntax understood by the core attachment parser. Always
/// quoting keeps paths with whitespace lossless; quotes and backslashes use the parser's two
/// explicit escape sequences.
fn format_at_mention(path: &str) -> String {
    let mut mention = String::with_capacity(path.len().saturating_add(4));
    mention.push_str("@\"");
    for character in path.chars() {
        if matches!(character, '\"' | '\\') {
            mention.push('\\');
        }
        mention.push(character);
    }
    mention.push_str("\" ");
    mention
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

async fn wait_for_model_save(
    handle: &mut Option<JoinHandle<ModelSaveOutcome>>,
) -> Result<ModelSaveOutcome, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_effort_save(
    handle: &mut Option<JoinHandle<EffortSaveOutcome>>,
) -> Result<EffortSaveOutcome, tokio::task::JoinError> {
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
        "╭" | "╮" | "╰" | "╯" | "┌" | "┐" | "└" | "┘" | "✓" => Some("+"),
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

    if area.width < 18 {
        let mark = if area.width >= 8 { "@%  GF" } else { "GF" };
        let mut lines = vec![Line::from(Span::styled(
            mark,
            Style::default()
                .fg(ACCENT_SOFT)
                .add_modifier(Modifier::BOLD),
        ))];
        if area.height >= 2 {
            lines.push(Line::from(Span::styled(
                "READY",
                Style::default().fg(SUCCESS),
            )));
        }
        let compact = centered_fixed(area, area.width.min(12), area.height.min(2));
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), compact);
        return;
    }

    if let Some((art, gap, copy_width)) = welcome_art_layout(area) {
        let copy_lines = welcome_copy(copy_width);
        let copy_height = u16::try_from(copy_lines.len()).unwrap_or(area.height);
        let hero_width = art
            .width()
            .saturating_add(gap)
            .saturating_add(copy_width)
            .min(area.width);
        let hero_height = art.height().max(copy_height).min(area.height);
        let hero = centered_fixed(area, hero_width, hero_height);
        let art_y = hero
            .y
            .saturating_add(hero.height.saturating_sub(art.height()) / 2);
        let art_area = Rect::new(hero.x, art_y, art.width(), art.height());
        render_brand_art(art_area, art, f);

        let copy_x = hero.x.saturating_add(art.width()).saturating_add(gap);
        let copy_y = hero
            .y
            .saturating_add(hero.height.saturating_sub(copy_height) / 2);
        let copy_area = Rect::new(copy_x, copy_y, copy_width, copy_height.min(hero.height));
        f.render_widget(Paragraph::new(copy_lines), copy_area);
        return;
    }

    let compact = centered_fixed(area, area.width.min(40), area.height.min(4));
    let lines = vec![
        Line::from(vec![
            Span::styled("@%#*+=  ", Style::default().fg(ACCENT_SOFT)),
            Span::styled(
                "GROKFORGE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "FORGE CORE / READY",
            Style::default().fg(ACCENT),
        )),
        Line::from(Span::styled(
            "Ask. Build. Verify.",
            Style::default().fg(TEXT),
        )),
        Line::from(vec![
            Span::styled("/", Style::default().fg(ACCENT_SOFT)),
            Span::styled(" deck  ·  ENTER cast", Style::default().fg(FAINT)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), compact);
}

fn render_startup_notice(area: Rect, notice: &str, f: &mut ratatui::Frame) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let line = Line::from(vec![
        Span::styled("▲  ", Style::default().fg(WARNING)),
        Span::styled(
            safe_terminal_line(notice),
            Style::default().fg(MUTED).bg(CANVAS),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(CANVAS)),
        first_row(area),
    );
}

fn push_startup_notice_lines(lines: &mut Vec<Line<'static>>, notice: &str, width: u16) {
    lines.push(Line::from(vec![
        Span::styled("▲  STARTUP  ", Style::default().fg(WARNING)),
        Span::styled(safe_terminal_line(notice), Style::default().fg(MUTED)),
    ]));
    if width > 0 {
        lines.push(Line::from(""));
    }
}

fn welcome_art_layout(area: Rect) -> Option<(&'static crate::brand::BrandArt, u16, u16)> {
    if area.width >= 116 && area.height >= 33 {
        return crate::brand::responsive(66, 33).map(|art| (art, 4, 46));
    }
    if area.width >= 72 && area.height >= 17 {
        return crate::brand::responsive(33, 17).map(|art| (art, 3, 36));
    }
    if area.width >= 43 && area.height >= 9 {
        return crate::brand::responsive(17, 9).map(|art| (art, 2, 24));
    }
    None
}

fn welcome_copy(width: u16) -> Vec<Line<'static>> {
    if width >= 34 {
        vec![
            Line::from(Span::styled(
                "@%#*+=  FORGE CORE / READY",
                Style::default().fg(ACCENT_SOFT),
            )),
            Line::from(Span::styled(
                "GROKFORGE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "LOCAL-FIRST TERMINAL INTELLIGENCE",
                Style::default().fg(MUTED),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "WHAT ARE WE BUILDING?",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("›  ", Style::default().fg(USER)),
                Span::styled("Trace a failure to its source", Style::default().fg(MUTED)),
            ]),
            Line::from(vec![
                Span::styled("›  ", Style::default().fg(ACCENT)),
                Span::styled(
                    "Ship a focused, verified change",
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("/", Style::default().fg(ACCENT_SOFT)),
                Span::styled("  OPEN THE FORGE DECK", Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("ENTER", Style::default().fg(ACCENT_SOFT)),
                Span::styled("  CAST THE PROMPT", Style::default().fg(FAINT)),
            ]),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "FORGE CORE / READY",
                Style::default().fg(ACCENT),
            )),
            Line::from(Span::styled(
                "GROKFORGE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Ask. Build. Verify.",
                Style::default().fg(MUTED),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("/", Style::default().fg(ACCENT_SOFT)),
                Span::styled("  command deck", Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("ENTER", Style::default().fg(ACCENT_SOFT)),
                Span::styled("  cast prompt", Style::default().fg(FAINT)),
            ]),
        ]
    }
}

fn render_brand_art(area: Rect, art: &crate::brand::BrandArt, f: &mut ratatui::Frame) {
    let lines = art
        .lines()
        .iter()
        .map(|line| styled_brand_line(line))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines), area);
}

fn styled_brand_line(value: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(first) = chars.next() {
        let color = brand_glyph_color(first);
        let mut run = String::from(first);
        while chars
            .peek()
            .is_some_and(|next| brand_glyph_color(*next) == color)
        {
            if let Some(next) = chars.next() {
                run.push(next);
            }
        }
        if let Some(color) = color {
            spans.push(Span::styled(run, Style::default().fg(color)));
        } else {
            spans.push(Span::raw(run));
        }
    }
    Line::from(spans)
}

fn brand_glyph_color(glyph: char) -> Option<Color> {
    match glyph {
        ' ' => None,
        '.' | ':' => Some(BORDER),
        '-' | '=' | '+' => Some(ACCENT_SOFT),
        '*' | '#' | '%' | '@' => Some(ACCENT),
        _ => Some(MUTED),
    }
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

fn push_markdown_body(lines: &mut Vec<Line<'static>>, text: &str, width: u16) {
    let indent = " ".repeat(usize::from(width).saturating_sub(1).min(3));
    let body_width = usize::from(width).saturating_sub(indent.len()).max(1);
    let rendered = render_markdown(text);

    for rendered_line in &rendered.lines {
        if rendered_line.kind == LineKind::Blank {
            lines.push(Line::from(""));
            continue;
        }

        let mut pieces = Vec::new();
        let mut structural_width = 0usize;
        if let Some((prefix, style)) = markdown_semantic_prefix(rendered_line) {
            structural_width = structural_width.saturating_add(display_width(prefix));
            pieces.push((prefix.to_string(), style));
        }
        let mut in_structural_prefix = true;
        for span in &rendered_line.spans {
            if in_structural_prefix
                && matches!(span.role, SpanRole::ListMarker | SpanRole::QuoteMarker)
            {
                structural_width = structural_width.saturating_add(display_width(&span.text));
            } else {
                in_structural_prefix = false;
            }
            pieces.push((span.text.clone(), markdown_span_style(rendered_line, span)));
        }

        structural_width = structural_width.min(body_width.saturating_sub(1));
        let continuation_width = body_width.saturating_sub(structural_width).max(1);
        for (index, row) in wrap_styled_pieces(pieces, body_width, continuation_width)
            .into_iter()
            .enumerate()
        {
            let mut spans = vec![Span::raw(indent.clone())];
            if index > 0 && structural_width > 0 {
                spans.push(Span::raw(" ".repeat(structural_width)));
            }
            spans.extend(
                row.into_iter()
                    .map(|(text, style)| Span::styled(text, style)),
            );
            lines.push(Line::from(spans));
        }
    }
}

fn markdown_semantic_prefix(line: &RenderLine) -> Option<(&'static str, Style)> {
    match line.kind {
        LineKind::Heading { level: 1 } => Some((
            "@ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        LineKind::Heading { level: 2 } => Some((
            "// ",
            Style::default()
                .fg(ACCENT_SOFT)
                .add_modifier(Modifier::BOLD),
        )),
        LineKind::Heading { .. } => Some((
            "· ",
            Style::default()
                .fg(ACCENT_SOFT)
                .add_modifier(Modifier::BOLD),
        )),
        LineKind::CodeBlock { .. } => Some(("│ ", Style::default().fg(BORDER))),
        LineKind::Truncation => Some(("! ", Style::default().fg(WARNING))),
        _ => None,
    }
}

fn markdown_span_style(line: &RenderLine, span: &RenderSpan) -> Style {
    let mut style = match line.kind {
        LineKind::Heading { level: 1 | 2 } => {
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
        }
        LineKind::Heading { .. } | LineKind::TableRow { header: true } => Style::default()
            .fg(ACCENT_SOFT)
            .add_modifier(Modifier::BOLD),
        LineKind::CodeBlock { .. } => Style::default().fg(TEXT).bg(SURFACE_RAISED),
        LineKind::TableSeparator | LineKind::ThematicBreak => Style::default().fg(BORDER),
        LineKind::Truncation => Style::default().fg(WARNING),
        _ => Style::default().fg(TEXT),
    };

    style = match span.role {
        SpanRole::ListMarker => style.fg(ACCENT_SOFT),
        SpanRole::QuoteMarker => style.fg(USER),
        SpanRole::TableDelimiter | SpanRole::ThematicBreak => style.fg(BORDER),
        SpanRole::Truncation => style.fg(WARNING),
        SpanRole::Text => style,
    };
    if span.style.strong {
        style = style.add_modifier(Modifier::BOLD);
    }
    if span.style.emphasis {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if span.style.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if span.style.code {
        style = style.fg(ACCENT_SOFT).bg(SURFACE_RAISED);
    }
    if span.link.is_some() {
        style = style.fg(USER).add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn wrap_styled_pieces(
    pieces: Vec<(String, Style)>,
    first_width: usize,
    continuation_width: usize,
) -> Vec<Vec<(String, Style)>> {
    let mut rows = Vec::new();
    let mut row = Vec::<(String, Style)>::new();
    let mut row_width = 0usize;
    let mut limit = first_width.max(1);

    for (text, style) in pieces {
        for ch in text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if !row.is_empty() && row_width.saturating_add(ch_width) > limit {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
                limit = continuation_width.max(1);
            }
            if let Some((run, run_style)) = row.last_mut()
                && *run_style == style
            {
                run.push(ch);
            } else {
                row.push((ch.to_string(), style));
            }
            row_width = row_width.saturating_add(ch_width);
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
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
            push_role_header(lines, "›", "YOU // INTENT", USER, None);
            let text = safe_terminal_text(text);
            push_body(lines, &text, TEXT, width);
        }
        Entry::Assistant(text) => {
            push_role_header(lines, "◆", "GROK // FORGE", ACCENT, None);
            push_markdown_body(lines, text, width);
        }
        Entry::Reasoning(text) => {
            lines.push(Line::from(vec![
                Span::styled("◇  TRACE  ", Style::default().fg(FAINT)),
                Span::styled(compact_preview(text, 112), Style::default().fg(MUTED)),
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
    if !matches!(entry, Entry::Tool { .. } | Entry::Reasoning(_)) {
        lines.push(Line::from(""));
    }
}

/// Derive a compact, sanitized activity string for a subagent lane from one of its inner events.
/// Returns `None` for events that carry no useful lane status.
fn subagent_activity(inner: &EventMsg) -> Option<String> {
    let text = match inner {
        EventMsg::ToolCallBegin {
            name, args_preview, ..
        } => humanize_tool_call(name, args_preview),
        EventMsg::ToolCallEnd { ok, .. } => {
            if *ok { "tool done" } else { "tool failed" }.to_string()
        }
        EventMsg::ReasoningDelta { .. } => "thinking…".to_string(),
        EventMsg::AgentMessageDelta { .. } | EventMsg::AgentMessageDone { .. } => {
            "writing…".to_string()
        }
        EventMsg::StreamRetrying { attempt, .. } => format!("retrying (attempt {attempt})"),
        EventMsg::Committed { .. } => "committed".to_string(),
        EventMsg::Error { message, .. } => format!("error: {message}"),
        _ => return None,
    };
    Some(bounded_text(&safe_terminal_line(&text), 100))
}

/// Pad with spaces or truncate (with an ellipsis) to exactly `width` terminal cells.
fn pad_or_truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let value_width = display_width(value);
    if value_width <= width {
        let mut out = value.to_string();
        out.push_str(&" ".repeat(width - value_width));
        out
    } else {
        let content_width = width.saturating_sub(1);
        let mut out = String::new();
        let mut used = 0usize;
        for character in value.chars() {
            let character_width = character.width().unwrap_or(0);
            if used.saturating_add(character_width) > content_width {
                break;
            }
            out.push(character);
            used = used.saturating_add(character_width);
        }
        out.push('…');
        out.push_str(&" ".repeat(width.saturating_sub(used.saturating_add(1))));
        out
    }
}

/// Human-friendly token count, e.g. `950`, `1.2k`, `3.4M`. Uses integer math to avoid float
/// precision casts on large counts.
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000)
    } else if n >= 1_000 {
        format!("{}.{}k", n / 1_000, (n % 1_000) / 100)
    } else {
        format!("{n}")
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

fn project_command_description(command: &CommandDoc) -> String {
    let summary = command
        .template
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Run this project's saved workflow")
        .trim_start_matches(['#', '-', '*', '>', ' '])
        .trim();
    let summary = if summary.is_empty() {
        "Run this project's saved workflow".to_string()
    } else {
        compact_preview(summary, 96)
    };
    format!("Project · {summary}")
}

fn slash_palette_line(
    item: &SlashPaletteItem,
    chosen: bool,
    name_width: usize,
    show_description: bool,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            if chosen { "◆ " } else { "  " },
            Style::default().fg(if chosen { ACCENT } else { FAINT }),
        ),
        Span::styled(
            format!("{:<name_width$}", item.completion),
            Style::default()
                .fg(if chosen { TEXT } else { ACCENT_SOFT })
                .add_modifier(if chosen {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ];
    if show_description {
        spans.push(Span::styled(
            format!("  {}", item.description),
            Style::default().fg(if chosen { TEXT } else { MUTED }),
        ));
    }
    Line::from(spans).style(Style::default().bg(if chosen { SURFACE_RAISED } else { SURFACE }))
}

fn slash_toolbox_line(width: usize) -> Line<'static> {
    let tools = if width >= 74 {
        "read · write · edit · list · glob · grep · shell · git · task"
    } else if width >= 48 {
        "read · edit · search · shell · git · task"
    } else {
        "open /tools"
    };
    Line::from(vec![
        Span::styled(
            "  LOCAL TOOLS  ",
            Style::default().fg(TOOL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(tools, Style::default().fg(MUTED)),
    ])
    .style(Style::default().bg(SURFACE))
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

    if modal.width < 3 || modal.height < 3 {
        f.render_widget(Paragraph::new(compact_approval_controls()), modal);
        return;
    }

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
    if inner.height <= 2 {
        let controls_area = first_row(inner);
        f.render_widget(Paragraph::new(compact_approval_controls()), controls_area);
        if inner.height == 2 {
            let reason_area = Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("WHY  ", Style::default().fg(WARNING)),
                    Span::styled(
                        safe_terminal_line(&pending.request.reason),
                        Style::default().fg(TEXT),
                    ),
                ])),
                reason_area,
            );
        }
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
                Span::styled(" Esc ", Style::default().fg(CANVAS).bg(DANGER)),
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
        Paragraph::new(compact_approval_controls())
    };
    f.render_widget(controls, chunks[3]);
}

fn compact_approval_controls() -> Line<'static> {
    Line::from(vec![
        Span::styled("Esc deny", Style::default().fg(DANGER)),
        Span::styled("  ·  ", Style::default().fg(FAINT)),
        Span::styled("y yes", Style::default().fg(SUCCESS)),
        Span::styled("  ·  ", Style::default().fg(FAINT)),
        Span::styled("a all", Style::default().fg(WARNING)),
    ])
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
        .filter(|ch| is_safe_terminal_char(*ch))
        .collect()
}

fn is_safe_terminal_char(ch: char) -> bool {
    matches!(ch, '\n' | '\t')
        || (!ch.is_control() && !matches!(ch as u32, 0x7f..=0x9f) && !is_bidi_control(ch))
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

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use futures::FutureExt as _;
    use grokforge_core::{Agent, Session, SessionConfig, ToolRegistry};
    use grokforge_protocol::{Decision, DenialClass, EventMsg, LedgerEntry, ResponseItem, Usage};
    use grokforge_sandbox::PassthroughRunner;
    use grokforge_xai::{Effort, ModelInfo, XaiClient};
    // `ServerTool` is only exercised by the Unix-gated project-capabilities test.
    #[cfg(unix)]
    use grokforge_xai::ServerTool;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::text::Line;
    use tokio::sync::mpsc;

    use super::{
        AgentLane, App, ApprovalDetail, DisplayMode, EffortSaveOutcome, Entry, LaneStatus,
        MAX_COMPOSER_BYTES, MOUSE_SCROLL_ROWS, ModelSaveOutcome, PlanConfigRestore, TurnOutcome,
        format_at_mention,
    };
    use crate::approver::ChannelApprover;

    fn test_app() -> App {
        test_app_with_history(Vec::new())
    }

    #[test]
    fn attachment_picker_quotes_and_escapes_selected_paths() {
        assert_eq!(
            format_at_mention("docs/design notes.md"),
            "@\"docs/design notes.md\" "
        );
        assert_eq!(
            format_at_mention("docs/a\"quote\\name.md"),
            "@\"docs/a\\\"quote\\\\name.md\" "
        );

        let mut app = test_app();
        app.composer = "inspect @design".to_string();
        app.complete_at_item("docs/design notes.md");
        assert_eq!(app.composer, "inspect @\"docs/design notes.md\" ");
        assert!(app.at_selection.is_none());
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn arrow_keys_recall_prompt_history() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = test_app();
        app.record_history("first prompt");
        app.record_history("second prompt");

        // Up walks from newest to oldest; the first Up saves the (empty) draft.
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.composer, "second prompt");
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.composer, "first prompt");
        app.on_key(KeyEvent::from(KeyCode::Up)); // clamped at the oldest
        assert_eq!(app.composer, "first prompt");

        // Down walks back toward newest, then past it restores the draft.
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.composer, "second prompt");
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.composer, "");
        assert!(app.history_cursor.is_none());
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

    fn advertised_model(id: &str, aliases: &[&str], context_window: u64) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            created: None,
            owned_by: Some("xai".to_string()),
            aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
            context_window: Some(context_window),
        }
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
        assert!(frame.contains("TRACE"));
    }

    #[test]
    fn welcome_does_not_claim_an_unverified_network_connection() {
        let frame = buffer_frame(&test_app(), 80, 24);
        assert!(frame.contains("FORGE CORE / READY"));
        assert!(!frame.to_lowercase().contains("connected"));
    }

    #[test]
    fn welcome_art_selects_exact_responsive_rasters() {
        for (width, height, art_width, art_height) in
            [(116, 33, 66, 33), (72, 17, 33, 17), (43, 9, 17, 9)]
        {
            let area = ratatui::layout::Rect::new(0, 0, width, height);
            let (art, _, _) = super::welcome_art_layout(area).expect("responsive brand art");
            assert_eq!((art.width(), art.height()), (art_width, art_height));
        }
        assert!(super::welcome_art_layout(ratatui::layout::Rect::new(0, 0, 42, 8)).is_none());
    }

    #[test]
    fn welcome_identity_yields_cleanly_to_the_first_message() {
        let mut app = test_app();
        let welcome = buffer_frame(&app, 168, 88);
        assert!(welcome.contains("LOCAL-FIRST TERMINAL INTELLIGENCE"));
        assert!(welcome.contains("WHAT ARE WE BUILDING?"));

        app.transcript.push(Entry::User(
            "Make this terminal unmistakably ours".to_string(),
        ));
        let conversation = buffer_frame(&app, 168, 88);
        assert!(!conversation.contains("LOCAL-FIRST TERMINAL INTELLIGENCE"));
        assert!(!conversation.contains("WHAT ARE WE BUILDING?"));
        assert!(conversation.contains("Make this terminal unmistakably ours"));
    }

    #[test]
    fn startup_safety_notice_does_not_suppress_the_ascii_welcome() {
        let mut app = test_app();
        app.set_startup_notice(
            "auto-commit disabled: workspace had pre-existing uncommitted changes",
        );

        let frame = buffer_frame(&app, 168, 88);
        assert!(frame.contains("LOCAL-FIRST TERMINAL INTELLIGENCE"));
        assert!(frame.contains("auto-commit disabled"));
        let art = crate::brand::responsive(66, 33).expect("full bundled brand art");
        assert!(
            art.lines()
                .iter()
                .map(|line| line.trim())
                .filter(|fragment| fragment.len() >= 8)
                .any(|fragment| frame.contains(fragment)),
            "launch frame did not contain the bundled ASCII mark:\n{frame}"
        );

        app.transcript
            .push(Entry::User("Start the audit".to_string()));
        let conversation = buffer_frame(&app, 168, 88);
        assert!(!conversation.contains("LOCAL-FIRST TERMINAL INTELLIGENCE"));
        assert!(conversation.contains("auto-commit disabled"));
        assert!(conversation.contains("Start the audit"));
    }

    #[test]
    fn parallel_agent_rows_fit_narrow_terminals_and_wide_unicode() {
        let app = test_app();
        let lane = AgentLane {
            id: "lane-1".to_string(),
            label: "分析界面 and verify the renderer".to_string(),
            index: 0,
            total: 32,
            activity: "检查 very long activity".to_string(),
            tokens: 12_345,
            status: LaneStatus::Running,
        };

        for width in [22, 30, 42, 80] {
            let row = app.agent_lane_line(&lane, width);
            assert!(
                row.width() <= usize::from(width),
                "agent row used {} cells in a {width}-cell panel: {row:?}",
                row.width()
            );
        }
    }

    #[test]
    fn brand_chrome_adapts_without_chopped_wordmarks() {
        for width in [1, 2, 8, 12, 16, 18, 24, 48, 72, 116] {
            for height in [1, 2, 4, 5, 8, 16, 24, 33] {
                let frame = buffer_frame(&test_app(), width, height);
                assert_eq!(frame.lines().count(), usize::from(height));
                if width < 18 {
                    assert!(!frame.contains("GROKFORG"), "{width}×{height}:\n{frame}");
                }
            }
        }
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
        assert!(frame.contains("FORGE CORE / READY"));
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
        assert!(welcome.contains("FORGE CORE / READY"));
        assert!(welcome.contains("LOCAL-FIRST TERMINAL INTELLIGENCE"));
        assert!(welcome.contains("CAST"));
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

    #[cfg(unix)] // discovery uses the Unix-only confined reader
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
    fn model_command_uses_the_startup_catalog() {
        let mut app = test_app();
        app.session
            .as_mut()
            .expect("session")
            .config
            .model_catalog
            .push(ModelInfo {
                id: "grok-4.5".to_string(),
                created: None,
                owned_by: Some("xai".to_string()),
                aliases: vec!["grok-4.5-latest".to_string()],
                context_window: Some(500_000),
            });

        app.handle_slash("model");
        assert!(
            app.transcript
                .iter()
                .any(|entry| matches!(entry, Entry::Info(text) if text.contains("grok-4.5")))
        );

        app.running = true;
        app.finish_model_save(Ok(ModelSaveOutcome {
            model: "grok-4.5".to_string(),
            context_window: Some(500_000),
            result: Ok(()),
        }));
        let session = app.session.as_ref().expect("session");
        assert_eq!(session.config.model, "grok-4.5");
        assert_eq!(session.config.context_window_tokens, Some(500_000));
        assert_eq!(app.status_model, "grok-4.5");
        assert!(!app.running);
    }

    #[test]
    fn incompatible_effort_and_model_switches_are_rejected_before_persistence() {
        let mut app = test_app();
        app.handle_slash("effort xhigh");
        assert_eq!(app.session.as_ref().expect("session").config.effort, None);
        assert!(app.effort_handle.is_none());
        assert!(
            app.transcript.iter().any(
                |entry| matches!(entry, Entry::Info(text) if text.contains("multi-agent model"))
            )
        );

        let session = app.session.as_mut().expect("session");
        session.config.model = "grok-fast-multi-agent".to_string();
        session.config.effort = Some(Effort::Xhigh);
        session
            .config
            .model_catalog
            .push(advertised_model("grok-4.5", &[], 500_000));
        app.handle_model("grok-4.5");
        let session = app.session.as_ref().expect("session");
        assert_eq!(session.config.model, "grok-fast-multi-agent");
        assert_eq!(session.config.effort, Some(Effort::Xhigh));
        assert!(app.model_handle.is_none());
    }

    #[test]
    fn successful_effort_save_updates_memory_only_after_persistence() {
        let mut app = test_app();
        assert_eq!(app.session.as_ref().expect("session").config.effort, None);
        app.running = true;
        app.finish_effort_save(Ok(EffortSaveOutcome {
            effort: Some(Effort::High),
            label: "high".to_string(),
            result: Ok(()),
        }));
        assert_eq!(
            app.session.as_ref().expect("session").config.effort,
            Some(Effort::High)
        );
        assert!(!app.running);
    }

    #[test]
    fn plan_routing_installs_catalog_model_context_and_high_then_restores() {
        let mut app = test_app();
        let session = app.session.as_mut().expect("session");
        session.config.model = "grok-build-0.1".to_string();
        session.config.plan_model = "plan-latest".to_string();
        session.config.context_window_tokens = Some(256_000);
        session.config.effort = Some(Effort::Low);
        session
            .config
            .model_catalog
            .push(advertised_model("grok-4.5", &["plan-latest"], 500_000));

        let restore = PlanConfigRestore::apply(session).expect("plan model");
        assert_eq!(session.config.model, "grok-4.5");
        assert_eq!(session.config.context_window_tokens, Some(500_000));
        assert_eq!(session.config.effort, Some(Effort::High));

        restore.restore(session);
        assert_eq!(session.config.model, "grok-build-0.1");
        assert_eq!(session.config.context_window_tokens, Some(256_000));
        assert_eq!(session.config.effort, Some(Effort::Low));
    }

    #[tokio::test]
    async fn plan_routing_restores_all_fields_after_a_caught_panic() {
        let mut app = test_app();
        let session = app.session.as_mut().expect("session");
        session.config.plan_model = "grok-4.5".to_string();
        session.config.context_window_tokens = Some(256_000);
        session.config.effort = None;
        session
            .config
            .model_catalog
            .push(advertised_model("grok-4.5", &[], 500_000));
        let original_model = session.config.model.clone();

        let restore = PlanConfigRestore::apply(session).expect("plan model");
        let panic = std::panic::AssertUnwindSafe(async { panic!("plan failed") })
            .catch_unwind()
            .await;
        assert!(panic.is_err());
        restore.restore(session);

        assert_eq!(session.config.model, original_model);
        assert_eq!(session.config.context_window_tokens, Some(256_000));
        assert_eq!(session.config.effort, None);
    }

    #[test]
    fn plan_routing_rejects_an_unadvertised_configured_model() {
        let mut app = test_app();
        let session = app.session.as_mut().expect("session");
        session.config.plan_model = "retired-plan-model".to_string();
        session
            .config
            .model_catalog
            .push(advertised_model("grok-4.5", &[], 500_000));
        let original_model = session.config.model.clone();

        assert!(PlanConfigRestore::apply(session).is_err());
        assert_eq!(session.config.model, original_model);
        assert_eq!(session.config.effort, None);
    }

    #[test]
    fn foreground_undo_is_honest_instead_of_spawning_a_noop_git_task() {
        let mut app = test_app();

        app.handle_slash("undo");

        assert!(!app.running);
        assert!(app.undo_handle.is_none());
        assert!(app.transcript.iter().any(
            |entry| matches!(entry, Entry::Info(text) if text.contains("foreground undo is not available"))
        ));
    }

    #[cfg(unix)] // discovery uses the Unix-only confined reader
    #[test]
    fn slash_palette_filters_builtins_and_project_commands_live() {
        use crossterm::event::{KeyCode, KeyEvent};

        let workspace = tempfile::tempdir().expect("workspace");
        let commands = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&commands).expect("commands directory");
        std::fs::write(
            commands.join("review.md"),
            "Review the current diff carefully.\nDo not expose this second line.",
        )
        .expect("command");
        let mut app = test_app_in(workspace.path().to_path_buf(), Vec::new());

        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        let all = buffer_frame(&app, 96, 24);
        assert!(all.contains("FORGE DECK"), "{all}");
        assert!(all.contains("/tools"), "{all}");
        assert!(all.contains("capability deck"), "{all}");
        assert!(all.contains("LOCAL TOOLS"), "{all}");
        assert!(all.contains("read · write · edit"), "{all}");

        for ch in "rev".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        let filtered = buffer_frame(&app, 96, 24);
        assert!(filtered.contains("/review"), "{filtered}");
        assert!(filtered.contains("Project · Review the current diff carefully."));
        assert!(!filtered.contains("Command map and keyboard shortcuts"));
        assert!(!filtered.contains("second line"));
    }

    #[cfg(unix)] // discovery uses the Unix-only confined reader
    #[test]
    fn slash_palette_keyboard_navigation_completes_and_executes() {
        use crossterm::event::{KeyCode, KeyEvent};

        let workspace = tempfile::tempdir().expect("workspace");
        let commands = workspace.path().join(".grokforge/commands");
        std::fs::create_dir_all(&commands).expect("commands directory");
        std::fs::write(commands.join("review.md"), "Review the current diff.").expect("command");
        let mut app = test_app_in(workspace.path().to_path_buf(), Vec::new());

        for ch in "/rev".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.composer, "/review ");

        app.composer = "/to".to_string();
        app.on_composer_changed();
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.composer.is_empty());
        assert!(
            app.session
                .as_ref()
                .expect("session")
                .config
                .enabled_server_tools
                .contains(&ServerTool::WebSearch)
        );

        app.composer = "/pl".to_string();
        app.on_composer_changed();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.composer, "/plan ");
        assert!(!app.running);
    }

    #[test]
    fn slash_palette_escape_dismisses_without_stealing_normal_navigation() {
        use crossterm::event::{KeyCode, KeyEvent};

        let mut app = test_app();
        app.composer = "/".to_string();
        app.on_composer_changed();
        let narrow = buffer_frame(&app, 10, 8);
        assert_eq!(narrow.lines().count(), 8, "{narrow}");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.slash_selection.is_none());
        assert!(!buffer_frame(&app, 80, 24).contains("/ FORGE DECK ·"));
        // The arrows now recall prompt history; the transcript scrolls with PageUp/PageDown.
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.scroll, 10);

        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert!(
            app.slash_selection.is_some(),
            "typing should reopen results"
        );
        assert!(buffer_frame(&app, 80, 24).contains("/help"));
    }

    #[test]
    fn tools_command_is_an_immediate_local_capability_deck() {
        let mut app = test_app();
        app.handle_slash("tools");
        let frame = buffer_frame(&app, 100, 32);
        assert!(frame.contains("LOCAL CAPABILITIES"), "{frame}");
        assert!(frame.contains("read · write · edit · list · glob · grep · shell"));
        assert!(frame.contains("git status · git diff · spawn task"));
        assert!(frame.contains("xAI-HOSTED TOOLS"));
    }

    #[test]
    fn idle_exit_aliases_and_empty_ctrl_d_quit_locally() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for alias in ["exit", "QUIT", "  Exit  ", "e\u{202e}xit"] {
            let mut app = test_app();
            app.composer = alias.to_string();
            app.submit();
            assert_eq!(app.shutdown, super::ShutdownState::Ready, "{alias}");
            assert!(app.transcript.is_empty(), "{alias} reached the transcript");
        }

        let mut app = test_app();
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.shutdown, super::ShutdownState::Ready);

        let mut app = test_app();
        app.composer = "keep this draft".to_string();
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.shutdown, super::ShutdownState::Active);
        assert_eq!(app.composer, "keep this draft");
    }

    #[test]
    fn project_command_summaries_cannot_inject_terminal_controls() {
        let command = grokforge_core::commands::CommandDoc {
            name: "review".to_string(),
            path: PathBuf::from("review.md"),
            template: "\u{1b}[31mReview \u{202e}hidden\nsecond line".to_string(),
        };
        let description = super::project_command_description(&command);
        assert!(description.contains("Review hidden"), "{description}");
        assert!(!description.contains('\u{1b}'));
        assert!(!description.contains('\u{202e}'));
        assert!(!description.contains('\n'));
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
        assert!(text.contains("FORGE CORE / READY"));
        assert!(text.contains("grok-build-0.1"));
        assert!(text.contains("/ opens tools + commands"));
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
    fn assistant_markdown_renders_as_terminal_ui_instead_of_raw_source() {
        let mut app = test_app();
        app.transcript.push(Entry::Assistant(
            "### Core **tools**\n\n| Tool | Purpose |\n| --- | --- |\n| `read` | Read **files** |"
                .to_string(),
        ));
        let frame = buffer_frame(&app, 100, 32);
        assert!(frame.contains("· Core tools"), "{frame}");
        assert!(frame.contains("Tool  │  Purpose"), "{frame}");
        assert!(frame.contains("read  │  Read files"), "{frame}");
        assert!(!frame.contains("###"), "{frame}");
        assert!(!frame.contains("**"), "{frame}");
        assert!(!frame.contains("| ---"), "{frame}");
    }

    #[test]
    fn protocol_activity_is_visible_in_transcript_and_status() {
        let mut app = test_app();
        app.running = true;
        app.on_agent_event(EventMsg::ReasoningDelta {
            delta: "Checking the retry path before editing.".to_string(),
        });
        assert!(buffer_text(&app, 100, 24).contains("TRACE"));

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
        assert!(frame.contains("◇  TRACE"));
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

    #[tokio::test]
    async fn concurrent_approval_requests_are_resolved_in_arrival_order() {
        use crossterm::event::{KeyCode, KeyEvent};
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        let (first_respond, first_wait) = oneshot::channel();
        app.on_approval_request(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/first".into(),
                },
                reason: "first concurrent request".to_string(),
            },
            respond: first_respond,
        });
        let (second_respond, second_wait) = oneshot::channel();
        app.on_approval_request(crate::approver::PendingApproval {
            request: ApprovalRequest {
                id: ApprovalId::new(),
                call_id: None,
                kind: ApprovalKind::WriteFile {
                    path: "/tmp/second".into(),
                },
                reason: "second concurrent request".to_string(),
            },
            respond: second_respond,
        });

        assert_eq!(
            app.pending
                .as_ref()
                .map(|pending| pending.request.reason.as_str()),
            Some("first concurrent request")
        );
        assert_eq!(app.approval_queue.len(), 1);
        assert!(
            app.approval_detail
                .as_ref()
                .is_some_and(|detail| detail.text.contains("first concurrent request"))
        );

        app.on_approval_key(KeyEvent::from(KeyCode::Char('y')));
        assert_eq!(first_wait.await.expect("first decision"), Decision::Approve);
        assert_eq!(
            app.pending
                .as_ref()
                .map(|pending| pending.request.reason.as_str()),
            Some("second concurrent request")
        );
        assert!(app.approval_queue.is_empty());
        assert!(
            app.approval_detail
                .as_ref()
                .is_some_and(|detail| detail.text.contains("second concurrent request"))
        );

        app.on_approval_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(second_wait.await.expect("second decision"), Decision::Deny);
        assert!(app.pending.is_none());
        assert!(app.approval_queue.is_empty());
        assert!(app.approval_detail.is_none());
    }

    #[test]
    fn narrow_approval_always_keeps_the_safe_deny_action_visible() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        for width in [12, 20, 30] {
            for height in 1..=8 {
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
                app.approval_detail =
                    Some(ApprovalDetail::new("request: write /tmp/x".to_string()));
                let frame = buffer_frame(&app, width, height);
                assert!(
                    frame.contains("Esc deny"),
                    "{width}×{height} frame:\n{frame}"
                );
                assert_eq!(frame.lines().count(), usize::from(height));
            }
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
    fn mouse_wheel_scrolls_transcript_and_resumes_follow_at_latest() {
        let mut app = test_app();
        app.transcript
            .push(Entry::Assistant("scrollable transcript".to_string()));

        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert!(!app.follow);
        assert_eq!(app.scroll, MOUSE_SCROLL_ROWS);

        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll, MOUSE_SCROLL_ROWS * 2);

        app.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert!(!app.follow);
        assert_eq!(app.scroll, MOUSE_SCROLL_ROWS);

        app.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert!(app.follow);
        assert_eq!(app.scroll, 0);

        // Follow mode owns offset zero; scrolling toward the latest transcript repairs stale state.
        app.scroll = 99;
        app.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert!(app.follow);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn mouse_wheel_is_bounded_and_welcome_screen_stays_in_follow_mode() {
        let mut app = test_app();
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert!(app.follow);
        assert_eq!(app.scroll, 0);

        app.streaming = Some("live response".to_string());
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert!(!app.follow);
        assert_eq!(app.scroll, MOUSE_SCROLL_ROWS);

        app.scroll = u16::MAX - 1;
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll, u16::MAX);
    }

    #[test]
    fn mouse_click_drag_and_motion_events_are_state_preserving() {
        let mut app = test_app();
        app.transcript
            .push(Entry::Assistant("existing transcript".to_string()));
        app.composer = "keep this draft".to_string();
        app.follow = false;
        app.scroll = 7;

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            app.on_mouse(mouse(kind));
        }

        assert_eq!(app.composer, "keep this draft");
        assert!(!app.follow);
        assert_eq!(app.scroll, 7);
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
        app.on_key(KeyEvent::new(KeyCode::Char('\u{202e}'), KeyModifiers::NONE));
        assert!(app.composer.is_empty());

        app.on_paste(&format!(
            "{}\u{1b}\u{202e}",
            "a".repeat(MAX_COMPOSER_BYTES + 100)
        ));
        assert_eq!(app.composer.len(), MAX_COMPOSER_BYTES);
        assert!(!app.composer.contains('\u{1b}'));
        assert!(!app.composer.contains('\u{202e}'));

        let mut drafting = test_app();
        drafting.running = true;
        drafting.on_paste("queue this next prompt");
        assert_eq!(drafting.composer, "queue this next prompt");
    }

    #[test]
    fn full_composer_drafts_keep_the_cursor_inside_the_viewport() {
        for width in [1, 2, 8, 10, 16, 40, 80] {
            let mut app = test_app();
            app.composer = "x".repeat(1_024);
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).expect("terminal");
            terminal.draw(|frame| app.render(frame)).expect("draw");
            let cursor = terminal.get_cursor_position().expect("cursor position");
            assert!(cursor.x < width, "{width}-column cursor: {cursor:?}");
            assert!(cursor.y < 8, "{width}-column cursor: {cursor:?}");
        }
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
        assert!(app.approval_queue.is_empty());
    }

    #[tokio::test]
    async fn quitting_aborts_the_visible_and_every_queued_approval() {
        use grokforge_protocol::{ApprovalId, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;

        let mut app = test_app();
        let (first_respond, first_wait) = oneshot::channel();
        let (second_respond, second_wait) = oneshot::channel();
        for (reason, respond) in [
            ("visible request", first_respond),
            ("queued request", second_respond),
        ] {
            app.on_approval_request(crate::approver::PendingApproval {
                request: ApprovalRequest {
                    id: ApprovalId::new(),
                    call_id: None,
                    kind: ApprovalKind::WriteFile {
                        path: format!("/tmp/{reason}").into(),
                    },
                    reason: reason.to_string(),
                },
                respond,
            });
        }

        assert!(app.pending.is_some());
        assert_eq!(app.approval_queue.len(), 1);
        app.request_quit();

        assert_eq!(first_wait.await.expect("visible decision"), Decision::Abort);
        assert_eq!(second_wait.await.expect("queued decision"), Decision::Abort);
        assert!(app.pending.is_none());
        assert!(app.approval_queue.is_empty());
        assert!(app.approval_detail.is_none());
    }
}
