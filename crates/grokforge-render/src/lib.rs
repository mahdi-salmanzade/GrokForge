//! Pure, terminal-safe presentation primitives for GrokForge output.
//!
//! This crate deliberately does not depend on a terminal UI framework. [`render_markdown`]
//! turns Markdown into semantic lines and spans which any frontend can map onto its own theme.
//! Parser input and renderer output are bounded, and every string crossing the API is stripped
//! of terminal control and bidirectional-override characters.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::sync::Arc;

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default maximum number of Markdown source bytes parsed for one render.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 512 * 1024;
/// Default maximum number of output lines produced for one render.
pub const DEFAULT_MAX_LINES: usize = 4_096;
/// Default maximum UTF-8 bytes in one rendered line.
pub const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024;
/// Default maximum number of semantic spans across the rendered result.
pub const DEFAULT_MAX_SPANS: usize = 32_768;

const REPLACEMENT: char = '\u{fffd}';
const TRUNCATION_TEXT: &str = "… output truncated …";
const MAX_VISIBLE_QUOTE_MARKERS: usize = 8;

/// Resource limits for a single Markdown render.
///
/// A zero limit is valid. It produces no corresponding output and marks the result truncated
/// when source data could not be represented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderLimits {
    /// Maximum source bytes handed to the Markdown parser.
    pub max_input_bytes: usize,
    /// Maximum lines in the returned result.
    pub max_lines: usize,
    /// Maximum UTF-8 bytes in each line, including visible list/quote markers.
    pub max_line_bytes: usize,
    /// Maximum spans across all returned lines.
    pub max_spans: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            max_spans: DEFAULT_MAX_SPANS,
        }
    }
}

/// A fully rendered, terminal-safe Markdown document.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderedMarkdown {
    /// Semantic output lines in source order.
    pub lines: Vec<RenderLine>,
    /// Whether any source or output was omitted because a configured limit was reached.
    pub truncated: bool,
}

impl RenderedMarkdown {
    /// Produce the safe, undecorated text represented by these semantic lines.
    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(RenderLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One terminal-oriented line of rendered Markdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderLine {
    /// The Markdown block which produced this line.
    pub kind: LineKind,
    /// Nested blockquote depth. Quote marker spans are also present for simple frontends.
    pub quote_depth: u16,
    /// Nested list depth. The first level is `1`.
    pub list_depth: u16,
    /// Marker attached to this line when it begins a list item.
    pub list_marker: Option<ListMarker>,
    /// Styled, terminal-safe spans in visual order.
    pub spans: Vec<RenderSpan>,
}

impl RenderLine {
    /// Concatenate this line's visible spans without terminal styling.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// Semantic block type for a rendered line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineKind {
    /// Ordinary prose.
    Paragraph,
    /// Heading content without the Markdown `#` markers.
    Heading {
        /// CommonMark heading level, from 1 through 6.
        level: u8,
    },
    /// One physical line from a fenced or indented code block.
    CodeBlock {
        /// Sanitized fenced-code information string, when present.
        language: Option<String>,
    },
    /// A horizontal rule.
    ThematicBreak,
    /// A readable table row with semantic delimiter spans.
    TableRow {
        /// Whether this is the table's header row.
        header: bool,
    },
    /// The visual separator immediately after a table header.
    TableSeparator,
    /// Intentional spacing between top-level paragraphs.
    Blank,
    /// A visible marker saying configured bounds omitted output.
    Truncation,
}

/// Markdown list marker metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListMarker {
    /// An unordered item.
    Bullet,
    /// An ordered item and its displayed number.
    Ordered(u64),
}

/// A terminal-safe piece of a rendered line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderSpan {
    /// Literal rendered text. It never contains terminal controls or bidi overrides.
    pub text: String,
    /// Inline Markdown presentation flags.
    pub style: SpanStyle,
    /// Sanitized link destination for linked labels.
    pub link: Option<Arc<str>>,
    /// Structural role, useful for applying a restrained frontend palette.
    pub role: SpanRole,
}

/// Inline Markdown presentation flags. Flags can be combined.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
// Markdown permits these independent properties to nest, so a state enum would lose combinations.
#[allow(clippy::struct_excessive_bools)]
pub struct SpanStyle {
    /// Markdown emphasis.
    pub emphasis: bool,
    /// Markdown strong emphasis.
    pub strong: bool,
    /// Markdown strikethrough.
    pub strikethrough: bool,
    /// Inline or fenced code.
    pub code: bool,
}

/// Structural role for a span.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpanRole {
    /// Ordinary document content.
    #[default]
    Text,
    /// The visible prefix for a list item or task checkbox.
    ListMarker,
    /// A visible blockquote rail.
    QuoteMarker,
    /// A table cell divider or header separator.
    TableDelimiter,
    /// A thematic break.
    ThematicBreak,
    /// A visible indication that output was bounded.
    Truncation,
}

/// Render Markdown using conservative default limits.
pub fn render_markdown(source: &str) -> RenderedMarkdown {
    render_markdown_with_limits(source, RenderLimits::default())
}

/// Render Markdown into frontend-independent semantic lines and spans.
///
/// The source is safely truncated on a UTF-8 boundary before parsing. Output is deterministic
/// for a given source and set of limits.
pub fn render_markdown_with_limits(source: &str, limits: RenderLimits) -> RenderedMarkdown {
    let (bounded, source_truncated) = bounded_source(source, limits.max_input_bytes);
    let mut renderer = Renderer::new(limits, source_truncated);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    for event in Parser::new_ext(bounded, options) {
        renderer.event(event);
        if renderer.stopped {
            break;
        }
    }

    renderer.finish()
}

/// Replace terminal controls and bidirectional formatting controls with the Unicode replacement
/// character. Tabs become four spaces so their width cannot depend on terminal tab stops.
/// Newlines are retained for callers sanitizing multi-line content.
pub fn sanitize_terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => safe.push('\n'),
            '\t' => safe.push_str("    "),
            '\r' => safe.push(REPLACEMENT),
            character if is_unsafe_terminal_character(character) => safe.push(REPLACEMENT),
            character => safe.push(character),
        }
    }
    safe
}

fn is_unsafe_terminal_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn sanitize_terminal_line(text: &str) -> String {
    sanitize_terminal_text(text).replace('\n', "�")
}

fn bounded_source(source: &str, max_bytes: usize) -> (&str, bool) {
    if source.len() <= max_bytes {
        return (source, false);
    }

    let mut end = max_bytes.min(source.len());
    while !source.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (&source[..end], true)
}

#[derive(Clone, Debug)]
struct LineBuilder {
    kind: LineKind,
    quote_depth: u16,
    list_depth: u16,
    list_marker: Option<ListMarker>,
    spans: Vec<RenderSpan>,
    bytes: usize,
}

impl LineBuilder {
    fn new(
        kind: LineKind,
        quote_depth: u16,
        list_depth: u16,
        list_marker: Option<ListMarker>,
    ) -> Self {
        Self {
            kind,
            quote_depth,
            list_depth,
            list_marker,
            spans: Vec::new(),
            bytes: 0,
        }
    }

    fn into_line(self) -> RenderLine {
        RenderLine {
            kind: self.kind,
            quote_depth: self.quote_depth,
            list_depth: self.list_depth,
            list_marker: self.list_marker,
            spans: self.spans,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ListState {
    next: Option<u64>,
}

impl ListState {
    fn take_marker(&mut self) -> ListMarker {
        match self.next {
            Some(number) => {
                self.next = Some(number.saturating_add(1));
                ListMarker::Ordered(number)
            }
            None => ListMarker::Bullet,
        }
    }
}

#[derive(Clone, Debug)]
struct CodeState {
    language: Option<String>,
    emitted_line: bool,
}

#[derive(Clone, Debug, Default)]
struct InlineState {
    emphasis: u16,
    strong: u16,
    strikethrough: u16,
    links: Vec<Arc<str>>,
}

impl InlineState {
    fn style(&self) -> SpanStyle {
        SpanStyle {
            emphasis: self.emphasis > 0,
            strong: self.strong > 0,
            strikethrough: self.strikethrough > 0,
            code: false,
        }
    }

    fn link(&self) -> Option<Arc<str>> {
        self.links.last().cloned()
    }
}

#[derive(Clone, Debug, Default)]
struct TableBuilder {
    rows: Vec<TableRow>,
    current_row: Option<TableRow>,
    current_cell: Option<Vec<RenderSpan>>,
    in_header: bool,
}

#[derive(Clone, Debug, Default)]
struct TableRow {
    header: bool,
    cells: Vec<Vec<RenderSpan>>,
}

struct Renderer {
    limits: RenderLimits,
    lines: Vec<RenderLine>,
    current: Option<LineBuilder>,
    inline: InlineState,
    heading: Option<u8>,
    quote_depth: u16,
    lists: Vec<ListState>,
    pending_list_marker: Option<ListMarker>,
    code: Option<CodeState>,
    table: Option<TableBuilder>,
    pending_blank: bool,
    span_count: usize,
    truncated: bool,
    stopped: bool,
}

impl Renderer {
    fn new(limits: RenderLimits, source_truncated: bool) -> Self {
        Self {
            limits,
            lines: Vec::new(),
            current: None,
            inline: InlineState::default(),
            heading: None,
            quote_depth: 0,
            lists: Vec::new(),
            pending_list_marker: None,
            code: None,
            table: None,
            pending_blank: false,
            span_count: 0,
            truncated: source_truncated,
            stopped: false,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if self.code.is_some() {
                    self.append_code_text(&text);
                } else {
                    self.append_inline(&text, false, SpanRole::Text);
                }
            }
            Event::Code(text) => self.append_inline(&text, true, SpanRole::Text),
            Event::InlineMath(text) => {
                self.append_inline("$", true, SpanRole::Text);
                self.append_inline(&text, true, SpanRole::Text);
                self.append_inline("$", true, SpanRole::Text);
            }
            Event::DisplayMath(text) => self.append_code_text(&text),
            Event::FootnoteReference(label) => {
                self.append_inline("[", false, SpanRole::Text);
                self.append_inline(&label, false, SpanRole::Text);
                self.append_inline("]", false, SpanRole::Text);
            }
            Event::SoftBreak => self.append_inline(" ", false, SpanRole::Text),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.thematic_break(),
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.append_inline(marker, false, SpanRole::ListMarker);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.finish_current();
                self.heading = Some(heading_level(level));
            }
            Tag::BlockQuote(_) => {
                self.finish_current();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.finish_current();
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(info) => {
                        let safe = sanitize_terminal_line(info.trim());
                        (!safe.is_empty()).then_some(safe)
                    }
                };
                self.code = Some(CodeState {
                    language,
                    emitted_line: false,
                });
            }
            Tag::List(start) => {
                self.finish_current();
                self.lists.push(ListState { next: start });
            }
            Tag::Item => {
                self.finish_current();
                self.pending_list_marker = self.lists.last_mut().map(ListState::take_marker);
            }
            Tag::Emphasis => self.inline.emphasis = self.inline.emphasis.saturating_add(1),
            Tag::Strong => self.inline.strong = self.inline.strong.saturating_add(1),
            Tag::Strikethrough => {
                self.inline.strikethrough = self.inline.strikethrough.saturating_add(1);
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.inline
                    .links
                    .push(Arc::from(sanitize_terminal_line(dest_url.as_ref())));
            }
            Tag::Table(_) => {
                self.finish_current();
                self.table = Some(TableBuilder::default());
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                    table.current_row = Some(TableRow {
                        header: true,
                        cells: Vec::new(),
                    });
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row = Some(TableRow {
                        header: table.in_header,
                        cells: Vec::new(),
                    });
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.current_cell = Some(Vec::new());
                }
            }
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.finish_current();
                if self.lists.is_empty() && self.quote_depth == 0 && self.table.is_none() {
                    self.pending_blank = true;
                }
            }
            TagEnd::Heading(_) => {
                self.finish_current();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.finish_current();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 && self.lists.is_empty() {
                    self.pending_blank = true;
                }
            }
            TagEnd::CodeBlock => {
                if self.current.is_some() {
                    self.finish_current();
                } else if self.code.as_ref().is_some_and(|code| !code.emitted_line) {
                    self.start_line();
                    self.finish_current();
                }
                self.code = None;
                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.pending_blank = true;
                }
            }
            TagEnd::List(_) => {
                self.finish_current();
                let _ = self.lists.pop();
                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.pending_blank = true;
                }
            }
            TagEnd::Item => {
                self.finish_current();
                self.pending_list_marker = None;
            }
            TagEnd::Emphasis => self.inline.emphasis = self.inline.emphasis.saturating_sub(1),
            TagEnd::Strong => self.inline.strong = self.inline.strong.saturating_sub(1),
            TagEnd::Strikethrough => {
                self.inline.strikethrough = self.inline.strikethrough.saturating_sub(1);
            }
            TagEnd::Link | TagEnd::Image => {
                let _ = self.inline.links.pop();
            }
            TagEnd::TableCell => self.finish_table_cell(),
            TagEnd::TableRow => self.finish_table_row(),
            TagEnd::TableHead => {
                self.finish_table_row();
                if let Some(table) = self.table.as_mut() {
                    table.in_header = false;
                }
            }
            TagEnd::Table => self.finish_table(),
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {}
        }
    }

    fn append_inline(&mut self, text: &str, code: bool, role: SpanRole) {
        if text.is_empty() || self.stopped {
            return;
        }

        let safe = sanitize_terminal_text(text).replace('\n', " ");
        if safe.is_empty() {
            return;
        }

        let mut style = self.inline.style();
        style.code = code;
        let link = self.inline.link();

        if let Some(table) = self.table.as_mut()
            && let Some(cell) = table.current_cell.as_mut()
        {
            push_or_merge_span(
                cell,
                RenderSpan {
                    text: safe,
                    style,
                    link,
                    role,
                },
            );
            return;
        }

        self.start_line();
        self.append_span(RenderSpan {
            text: safe,
            style,
            link,
            role,
        });
    }

    fn append_code_text(&mut self, text: &str) {
        if self.code.is_none() {
            self.append_inline(text, true, SpanRole::Text);
            return;
        }

        let mut start = 0;
        for (index, character) in text.char_indices() {
            if character == '\n' {
                self.append_code_segment(&text[start..index]);
                self.finish_current();
                if let Some(code) = self.code.as_mut() {
                    code.emitted_line = true;
                }
                start = index + character.len_utf8();
            }
        }
        if start < text.len() {
            self.append_code_segment(&text[start..]);
        }
    }

    fn append_code_segment(&mut self, text: &str) {
        let without_carriage_return = text.strip_suffix('\r').unwrap_or(text);
        self.append_inline(without_carriage_return, true, SpanRole::Text);
        if self.current.is_none() {
            self.start_line();
        }
    }

    fn hard_break(&mut self) {
        if self.table.is_some() {
            self.append_inline(" ", false, SpanRole::Text);
            return;
        }
        self.finish_current();
    }

    fn thematic_break(&mut self) {
        self.finish_current();
        self.start_specific_line(LineKind::ThematicBreak, None);
        self.append_span(RenderSpan {
            text: "────────".to_owned(),
            style: SpanStyle::default(),
            link: None,
            role: SpanRole::ThematicBreak,
        });
        self.finish_current();
    }

    fn start_line(&mut self) {
        if self.current.is_some() || self.stopped {
            return;
        }

        let kind = if let Some(code) = &self.code {
            LineKind::CodeBlock {
                language: code.language.clone(),
            }
        } else if let Some(level) = self.heading {
            LineKind::Heading { level }
        } else {
            LineKind::Paragraph
        };
        let marker = self.pending_list_marker.take();
        self.start_specific_line(kind, marker);
    }

    fn start_specific_line(&mut self, kind: LineKind, marker: Option<ListMarker>) {
        self.emit_pending_blank();
        if self.stopped {
            return;
        }

        let quote_depth = self.quote_depth;
        let list_depth = u16::try_from(self.lists.len()).unwrap_or(u16::MAX);
        self.current = Some(LineBuilder::new(kind, quote_depth, list_depth, marker));

        if quote_depth > 0 {
            let visible = usize::from(quote_depth).min(MAX_VISIBLE_QUOTE_MARKERS);
            self.append_span(RenderSpan {
                text: "› ".repeat(visible),
                style: SpanStyle::default(),
                link: None,
                role: SpanRole::QuoteMarker,
            });
        }
        if let Some(marker) = marker {
            let text = match marker {
                ListMarker::Bullet => "• ".to_owned(),
                ListMarker::Ordered(number) => format!("{number}. "),
            };
            self.append_span(RenderSpan {
                text,
                style: SpanStyle::default(),
                link: None,
                role: SpanRole::ListMarker,
            });
        }
    }

    fn emit_pending_blank(&mut self) {
        if !self.pending_blank || self.lines.is_empty() || self.stopped {
            self.pending_blank = false;
            return;
        }
        self.pending_blank = false;
        if self
            .lines
            .last()
            .is_some_and(|line| line.kind == LineKind::Blank)
        {
            return;
        }
        self.push_line(RenderLine {
            kind: LineKind::Blank,
            quote_depth: 0,
            list_depth: 0,
            list_marker: None,
            spans: Vec::new(),
        });
    }

    fn append_span(&mut self, mut span: RenderSpan) {
        if self.stopped || span.text.is_empty() {
            return;
        }

        let Some(line) = self.current.as_mut() else {
            return;
        };
        let available = self.limits.max_line_bytes.saturating_sub(line.bytes);
        if available == 0 {
            self.mark_stopped();
            return;
        }

        let original_len = span.text.len();
        truncate_string_to_bytes(&mut span.text, available);
        if span.text.is_empty() {
            self.mark_stopped();
            return;
        }

        line.bytes = line.bytes.saturating_add(span.text.len());
        let can_merge = line.spans.last().is_some_and(|last| {
            last.style == span.style && last.link == span.link && last.role == span.role
        });
        if can_merge {
            if let Some(last) = line.spans.last_mut() {
                last.text.push_str(&span.text);
            }
        } else if self.span_count < self.limits.max_spans {
            line.spans.push(span);
            self.span_count = self.span_count.saturating_add(1);
        } else {
            self.mark_stopped();
            return;
        }

        if line.bytes >= self.limits.max_line_bytes && original_len > available {
            self.mark_stopped();
        }
    }

    fn finish_current(&mut self) {
        let Some(line) = self.current.take() else {
            return;
        };
        if let Some(code) = self.code.as_mut() {
            code.emitted_line = true;
        }
        self.push_line(line.into_line());
    }

    fn push_line(&mut self, line: RenderLine) {
        if self.lines.len() >= self.limits.max_lines {
            self.mark_stopped();
            return;
        }
        self.lines.push(line);
    }

    fn mark_stopped(&mut self) {
        self.truncated = true;
        self.stopped = true;
    }

    fn finish_table_cell(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let cell = table.current_cell.take().unwrap_or_default();
        if let Some(row) = table.current_row.as_mut() {
            row.cells.push(cell);
        }
    }

    fn finish_table_row(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        if table.current_cell.is_some() {
            let cell = table.current_cell.take().unwrap_or_default();
            if let Some(row) = table.current_row.as_mut() {
                row.cells.push(cell);
            }
        }
        if let Some(row) = table.current_row.take() {
            table.rows.push(row);
        }
    }

    fn finish_table(&mut self) {
        self.finish_table_cell();
        self.finish_table_row();
        let Some(table) = self.table.take() else {
            return;
        };
        let widths = table_column_widths(&table.rows);

        for row in table.rows {
            if self.stopped {
                break;
            }
            let header = row.header;
            self.start_specific_line(LineKind::TableRow { header }, None);
            for (index, cell) in row.cells.into_iter().enumerate() {
                if index > 0 {
                    self.append_span(RenderSpan {
                        text: "  │  ".to_owned(),
                        style: SpanStyle::default(),
                        link: None,
                        role: SpanRole::TableDelimiter,
                    });
                }
                for mut span in cell {
                    if header {
                        span.style.strong = true;
                    }
                    self.append_span(span);
                }
            }
            self.finish_current();

            if header && !self.stopped {
                self.start_specific_line(LineKind::TableSeparator, None);
                let separator = widths
                    .iter()
                    .map(|width| "─".repeat((*width).clamp(3, 24)))
                    .collect::<Vec<_>>()
                    .join("──┼──");
                self.append_span(RenderSpan {
                    text: separator,
                    style: SpanStyle::default(),
                    link: None,
                    role: SpanRole::TableDelimiter,
                });
                self.finish_current();
            }
        }
        self.pending_blank = true;
    }

    fn finish(mut self) -> RenderedMarkdown {
        if !self.stopped {
            self.finish_current();
        }
        while self
            .lines
            .last()
            .is_some_and(|line| line.kind == LineKind::Blank)
        {
            let _ = self.lines.pop();
        }
        if self.truncated {
            self.install_truncation_marker();
        }
        RenderedMarkdown {
            lines: self.lines,
            truncated: self.truncated,
        }
    }

    fn install_truncation_marker(&mut self) {
        if self.limits.max_lines == 0 || self.limits.max_line_bytes == 0 {
            self.lines.clear();
            return;
        }
        let mut text = TRUNCATION_TEXT.to_owned();
        truncate_string_to_bytes(&mut text, self.limits.max_line_bytes);
        let replacing_last = self.lines.len() >= self.limits.max_lines;
        let retained_span_count: usize = if replacing_last {
            self.lines
                .iter()
                .take(self.lines.len().saturating_sub(1))
                .map(|line| line.spans.len())
                .sum()
        } else {
            self.lines.iter().map(|line| line.spans.len()).sum()
        };
        let marker = RenderLine {
            kind: LineKind::Truncation,
            quote_depth: 0,
            list_depth: 0,
            list_marker: None,
            spans: if retained_span_count >= self.limits.max_spans || text.is_empty() {
                Vec::new()
            } else {
                vec![RenderSpan {
                    text,
                    style: SpanStyle::default(),
                    link: None,
                    role: SpanRole::Truncation,
                }]
            },
        };

        if self.lines.len() < self.limits.max_lines {
            self.lines.push(marker);
        } else if let Some(last) = self.lines.last_mut() {
            *last = marker;
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn truncate_string_to_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.truncate(end);
}

fn push_or_merge_span(spans: &mut Vec<RenderSpan>, span: RenderSpan) {
    if let Some(last) = spans.last_mut()
        && last.style == span.style
        && last.link == span.link
        && last.role == span.role
    {
        last.text.push_str(&span.text);
    } else {
        spans.push(span);
    }
}

fn table_column_widths(rows: &[TableRow]) -> Vec<usize> {
    let columns = rows.iter().map(|row| row.cells.len()).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (column, cell) in row.cells.iter().enumerate() {
            let width = cell.iter().flat_map(|span| span.text.chars()).count();
            widths[column] = widths[column].max(width);
        }
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_has_version() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn renders_headings_and_nested_inline_styles_without_markdown_punctuation() {
        let rendered = render_markdown("### Build **bold and *careful*** with `cargo test`");

        assert_eq!(rendered.lines.len(), 1);
        assert_eq!(rendered.lines[0].kind, LineKind::Heading { level: 3 });
        assert_eq!(
            rendered.plain_text(),
            "Build bold and careful with cargo test"
        );
        assert!(
            rendered.lines[0]
                .spans
                .iter()
                .any(|span| { span.text == "careful" && span.style.strong && span.style.emphasis })
        );
        assert!(
            rendered.lines[0]
                .spans
                .iter()
                .any(|span| span.text == "cargo test" && span.style.code)
        );
    }

    #[test]
    fn renders_lists_quotes_tasks_and_paragraph_spacing() {
        let source = "> wisdom\n\n- first\n- [x] shipped\n\n1. one\n2. two\n\nlast";
        let rendered = render_markdown(source);
        let text = rendered.plain_text();

        assert!(text.contains("› wisdom"));
        assert!(text.contains("• first"));
        assert!(text.contains("• [x] shipped"));
        assert!(text.contains("1. one"));
        assert!(text.contains("2. two"));
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Blank)
        );
        assert_eq!(
            rendered
                .lines
                .iter()
                .find(|line| line.plain_text().contains("wisdom"))
                .map(|line| line.quote_depth),
            Some(1)
        );
    }

    #[test]
    fn fenced_code_keeps_literal_markdown_and_language() {
        let rendered = render_markdown("```rust\nlet raw = \"**not bold**\";\n\nrun();\n```");

        assert_eq!(rendered.lines.len(), 3);
        assert_eq!(
            rendered.lines[0].plain_text(),
            "let raw = \"**not bold**\";"
        );
        assert!(rendered.lines[1].plain_text().is_empty());
        assert_eq!(rendered.lines[2].plain_text(), "run();");
        assert!(rendered.lines.iter().all(|line| {
            matches!(
                &line.kind,
                LineKind::CodeBlock { language: Some(language) } if language == "rust"
            )
        }));
        assert!(rendered.lines[0].spans[0].style.code);
    }

    #[test]
    fn links_retain_safe_destination_and_label() {
        let rendered = render_markdown("Read [the docs](https://example.com/a?q=1).\n");
        let linked = rendered.lines[0]
            .spans
            .iter()
            .find(|span| span.text == "the docs");

        assert_eq!(
            linked.and_then(|span| span.link.as_deref()),
            Some("https://example.com/a?q=1")
        );
        assert_eq!(rendered.plain_text(), "Read the docs.");
    }

    #[test]
    fn tables_degrade_to_aligned_semantic_rows_without_raw_markdown() {
        let source = "| Tool | Purpose |\n| --- | --- |\n| `read` | Read **files** |\n| shell | Run commands |";
        let rendered = render_markdown(source);

        assert_eq!(rendered.lines.len(), 4);
        assert_eq!(rendered.lines[0].kind, LineKind::TableRow { header: true });
        assert_eq!(rendered.lines[1].kind, LineKind::TableSeparator);
        assert_eq!(rendered.lines[2].kind, LineKind::TableRow { header: false });
        assert_eq!(rendered.lines[3].kind, LineKind::TableRow { header: false });
        assert_eq!(rendered.lines[0].plain_text(), "Tool  │  Purpose");
        assert_eq!(rendered.lines[2].plain_text(), "read  │  Read files");
        assert_eq!(rendered.lines[3].plain_text(), "shell  │  Run commands");
        assert!(
            rendered.lines[2]
                .spans
                .iter()
                .any(|span| span.text == "read" && span.style.code)
        );
        assert!(
            rendered.lines[2]
                .spans
                .iter()
                .any(|span| span.text == "files" && span.style.strong)
        );
    }

    #[test]
    fn thematic_break_and_hard_break_are_semantic() {
        let rendered = render_markdown("above  \nbelow\n\n---\n");

        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.plain_text() == "above")
        );
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.plain_text() == "below")
        );
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.kind == LineKind::ThematicBreak)
        );
    }

    #[test]
    fn terminal_and_bidi_controls_are_never_returned() {
        let source = "ok\u{1b}[31m red\u{0007} x\u{202e}txt [go](https://safe/\u{2066}bad)";
        let rendered = render_markdown(source);

        assert!(!rendered.plain_text().contains('\u{1b}'));
        assert!(!rendered.plain_text().contains('\u{0007}'));
        assert!(!rendered.plain_text().contains('\u{202e}'));
        assert!(rendered.plain_text().contains(REPLACEMENT));
        assert!(
            rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| {
                    span.link
                        .as_deref()
                        .is_none_or(|link| !link.chars().any(is_unsafe_terminal_character))
                })
        );
    }

    #[test]
    fn sanitizer_expands_tabs_and_retains_newlines() {
        assert_eq!(
            sanitize_terminal_text("a\tb\nc\r"),
            format!("a    b\nc{REPLACEMENT}")
        );
    }

    #[test]
    fn utf8_input_limit_is_safe_and_visible() {
        let rendered = render_markdown_with_limits(
            "éééé",
            RenderLimits {
                max_input_bytes: 5,
                ..RenderLimits::default()
            },
        );

        assert!(rendered.truncated);
        assert!(rendered.plain_text().starts_with("éé"));
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Truncation)
        );
    }

    #[test]
    fn every_output_dimension_is_bounded() {
        let limits = RenderLimits {
            max_input_bytes: 80,
            max_lines: 3,
            max_line_bytes: 12,
            max_spans: 3,
        };
        let rendered = render_markdown_with_limits(
            "one **two** three four five\n\nsecond\n\nthird\n\nfourth",
            limits,
        );

        assert!(rendered.truncated);
        assert!(rendered.lines.len() <= limits.max_lines);
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| line.plain_text().len() <= limits.max_line_bytes)
        );
        assert!(
            rendered
                .lines
                .iter()
                .map(|line| line.spans.len())
                .sum::<usize>()
                <= limits.max_spans
        );
        assert_eq!(
            rendered.lines.last().map(|line| &line.kind),
            Some(&LineKind::Truncation)
        );
    }

    #[test]
    fn zero_limits_are_supported_without_panics() {
        let rendered = render_markdown_with_limits(
            "content",
            RenderLimits {
                max_input_bytes: 0,
                max_lines: 0,
                max_line_bytes: 0,
                max_spans: 0,
            },
        );

        assert!(rendered.truncated);
        assert!(rendered.lines.is_empty());
    }

    #[test]
    fn very_deep_quote_prefix_is_visually_bounded_but_depth_is_preserved() {
        let source = format!("{} text", "> ".repeat(100));
        let rendered = render_markdown(&source);
        let line = &rendered.lines[0];

        assert_eq!(line.quote_depth, 100);
        assert_eq!(
            line.spans
                .iter()
                .find(|span| span.role == SpanRole::QuoteMarker)
                .map(|span| span.text.as_str()),
            Some("› › › › › › › › ")
        );
    }
}
