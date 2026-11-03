//! Ratatui rendering for the [`crate::tui::App`] state. Pure functions from
//! state to widgets; no I/O, so views are testable with `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_markdown::{Options, StyleSheet};

use crate::tui::{App, Block as Msg, Phase, RenderedBlock, Toggle};
use crate::workspace::{
    NEW_BUTTON, SEPARATOR, TabMeta, TabStrip, Workspace, layout_strip, tab_title,
};

/// Lines of tool output shown below a tool row before collapsing.
const TOOL_PREVIEW_LINES: usize = 6;
/// Defensive cap on expanded tool output (the session model already trims).
const TOOL_MAX_LINES: usize = 500;

/// Colors for each transcript speaker.
fn label_style(label: &str) -> Style {
    match label {
        "you" => Style::new().cyan().bold(),
        "gewu" => Style::new().green().bold(),
        "error" => Style::new().red().bold(),
        _ => Style::new().dim(),
    }
}

/// Markdown styles aligned with the TUI palette: important text keeps the
/// terminal's default foreground, supporting detail is dimmed.
#[derive(Clone, Copy, Debug)]
struct GewuStyles;

impl StyleSheet for GewuStyles {
    fn heading(&self, level: u8) -> Style {
        // Mirror OpenCode's TUI: headings are accent-colored and bold, with
        // H1 additionally underlined (terminals have no font sizes).
        if level == 1 {
            Style::new().cyan().bold().underlined()
        } else {
            Style::new().cyan().bold()
        }
    }

    fn heading_marker(&self, _level: u8) -> &str {
        // Conceal `#`/`##` markers; the styled text carries the heading.
        ""
    }

    fn code(&self) -> Style {
        Style::new().cyan()
    }

    fn link(&self) -> Style {
        Style::new().blue().underlined()
    }

    fn blockquote(&self) -> Style {
        Style::new().dim()
    }

    fn list_marker(&self) -> Style {
        Style::new().dim()
    }

    fn table_header(&self) -> Style {
        Style::new().bold()
    }

    fn table_border(&self) -> Style {
        Style::new().dim()
    }
}

/// Transcript lines plus the click target of each line.
struct Lines {
    lines: Vec<Line<'static>>,
    clicks: Vec<Option<Toggle>>,
    width: u16,
}

impl Lines {
    fn new(width: u16) -> Self {
        Self {
            lines: Vec::new(),
            clicks: Vec::new(),
            width,
        }
    }

    fn push(&mut self, line: Line<'static>, click: Option<Toggle>) {
        self.lines.push(line);
        self.clicks.push(click);
    }

    fn blank(&mut self) {
        self.push(Line::from(""), None);
    }

    /// Columns available for text indented by `indent` (leaving a right margin).
    fn avail(&self, indent: usize) -> usize {
        (self.width as usize).saturating_sub(indent + 2).max(1)
    }

    /// Wrap `text` at `indent`, styling every line the same and making each
    /// one clickable when `click` is set.
    fn wrapped(&mut self, indent: usize, text: &str, style: Style, click: Option<Toggle>) {
        let pad = " ".repeat(indent);
        for raw in wrap_text(text, self.avail(indent)) {
            self.push(
                Line::from(Span::styled(format!("{pad}{raw}"), style)),
                click,
            );
        }
    }

    /// A two-tone row: dim `name`, default-color `title`, wrapped together.
    fn row(&mut self, indent: usize, name: &str, title: &str, click: Option<Toggle>) {
        let line = Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(name.to_string(), Style::new().dim()),
            Span::raw("  "),
            Span::raw(title.to_string()),
        ]);
        // `avail` leaves the right margin; wrapping counts the indent too.
        let width = self.avail(indent) + indent;
        if line.width() <= width {
            self.push(line, click);
        } else {
            for wrapped in wrap_line(&line, width) {
                self.push(wrapped, click);
            }
        }
    }
}

/// Render one transcript block as styled lines (label + body).
///
/// Finished assistant replies render as Markdown; the in-flight reply stays
/// raw so partially-typed syntax (e.g. an unclosed code fence) doesn't
/// flicker. Rendered Markdown is cached in `rendered` (indexed by block).
/// Tool blocks are handled by [`tool_block`] / [`tool_run`].
fn block_lines(
    block: &Msg,
    width: u16,
    streaming: bool,
    index: usize,
    rendered: &mut [Option<RenderedBlock>],
    out: &mut Lines,
) {
    let label = block.label();
    out.push(
        Line::from(Span::styled(label.to_string(), label_style(label))),
        None,
    );

    let body_width = width.saturating_sub(3).max(1) as usize;
    match block {
        // Tools are rendered by `tool_run`, which owns grouping and clicks.
        Msg::Tool { .. } => {}
        Msg::Assistant(text) if !streaming => {
            for line in markdown_block(rendered, index, text, body_width) {
                out.push(line, None);
            }
        }
        _ => {
            let text = block.text();
            // Important text (user prompts, streaming replies, errors) uses
            // the terminal's default foreground; only informational notes are
            // dimmed.
            let style = if matches!(block, Msg::Info(_)) {
                Style::new().dim()
            } else {
                Style::new()
            };
            for raw in wrap_text(text, body_width) {
                out.push(Line::from(Span::styled(format!("  {raw}"), style)), None);
            }
        }
    }
    out.blank();
}

/// Readable display name for a tool: the actual name, with only the verbose
/// aliases shortened (`read_file` → `read`). Words beat glyphs here — a row
/// reads `bash  git status` rather than `$ git status`.
fn tool_name(tool: &str) -> &str {
    match tool {
        "read_file" | "read" => "read",
        "write_file" | "write" => "write",
        "edit_file" | "edit" => "edit",
        "apply_patch" | "patch" => "patch",
        "webfetch" | "web_fetch" => "fetch",
        "websearch" | "web_search" => "search",
        "shell" => "bash",
        other => other,
    }
}

/// Name and title for a tool invocation row.
fn tool_row<'a>(tool: &'a str, input: &str) -> (&'a str, String) {
    let title = if input.is_empty() {
        // History can hold a tool call whose output never arrived.
        tool.to_string()
    } else {
        input.to_string()
    };
    (tool_name(tool), title)
}

/// One tool row (`bash  git status`) plus its output, collapsed unless
/// expanded. The name is dim; the command/path is default foreground so the
/// actionable part stands out.
fn tool_block(
    out: &mut Lines,
    index: usize,
    tool: &str,
    input: &str,
    output: Option<&str>,
    expanded: bool,
    indent: usize,
) {
    let (name, mut title) = tool_row(tool, input);
    if output.is_none() {
        title.push_str(" …");
    }
    let target = Some(Toggle::Output(index));
    out.row(indent, name, &title, target);

    let Some(output) = output else {
        return;
    };
    let lines: Vec<&str> = output.trim_end().lines().collect();
    if lines.is_empty() {
        return;
    }
    let shown = if expanded {
        lines.len().min(TOOL_MAX_LINES)
    } else {
        lines.len().min(TOOL_PREVIEW_LINES)
    };
    for line in &lines[..shown] {
        out.wrapped(indent + 2, line, Style::new().dim(), target);
    }
    if lines.len() > shown {
        let hidden = lines.len() - shown;
        out.wrapped(
            indent + 2,
            &format!("… +{hidden} line{}", if hidden == 1 { "" } else { "s" }),
            Style::new().dim(),
            target,
        );
    }
}

/// Render a run of consecutive tool blocks.
///
/// A single call renders as its own row. Two or more collapse into an
/// `Explored: …` summary row (OpenCode's grouped exploration) that reveals
/// the individual rows when expanded.
fn tool_run(
    out: &mut Lines,
    run: &[Msg],
    first: usize,
    group_expanded: bool,
    expanded_outputs: &std::collections::HashSet<usize>,
) {
    let tools: Vec<(&str, &str, Option<&str>)> = run
        .iter()
        .filter_map(|block| match block {
            Msg::Tool {
                tool,
                input,
                output,
            } => Some((tool.as_str(), input.as_str(), output.as_deref())),
            _ => None,
        })
        .collect();

    if tools.len() == 1 {
        let (tool, input, output) = tools[0];
        let expanded = expanded_outputs.contains(&first);
        tool_block(out, first, tool, input, output, expanded, 2);
        out.blank();
        return;
    }

    let glyph = if group_expanded { "−" } else { "+" };
    out.wrapped(
        2,
        &format!("{glyph} {}", group_label(run)),
        Style::new().dim(),
        Some(Toggle::Group(first)),
    );
    if group_expanded {
        for (offset, (tool, input, output)) in tools.iter().enumerate() {
            let index = first + offset;
            tool_block(
                out,
                index,
                tool,
                input,
                *output,
                expanded_outputs.contains(&index),
                4,
            );
        }
    }
    out.blank();
}

/// `Explored: 2 bash, 1 read` (or `Exploring: …` while a tool runs).
fn group_label(run: &[Msg]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut complete = true;
    for block in run {
        let Msg::Tool { tool, output, .. } = block else {
            continue;
        };
        if output.is_none() {
            complete = false;
        }
        let name = tool_name(tool);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(name, count)| format!("{count} {}", plural(name, *count)))
        .collect();
    format!(
        "{}: {}",
        if complete { "Explored" } else { "Exploring" },
        parts.join(", ")
    )
}

/// `1 read`, `2 reads`, `2 searches`, `2 bashes`? — no: `bash` stays `bash`.
fn plural(name: &str, count: usize) -> String {
    if count == 1 || name == "bash" {
        name.to_string()
    } else if name.ends_with('s') || name.ends_with("ch") || name.ends_with("sh") {
        format!("{name}es")
    } else {
        format!("{name}s")
    }
}

/// Render (or reuse a cached render of) one assistant message as Markdown.
fn markdown_block(
    cache: &mut [Option<RenderedBlock>],
    index: usize,
    text: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let hash = text_hash(text);
    let width = width.max(1) as u16;
    if let Some(Some(entry)) = cache.get(index)
        && entry.hash == hash
        && entry.width == width
    {
        return entry.lines.clone();
    }

    let lines = render_markdown(text, width);
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedBlock {
            hash,
            width,
            lines: lines.clone(),
        });
    }
    lines
}

/// Hash of a block's source text, so in-place rewrites invalidate the cache.
fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Markdown → wrapped, two-space-indented terminal lines.
fn render_markdown(text: &str, width: u16) -> Vec<Line<'static>> {
    let options = Options::new(GewuStyles).table_width(width);
    let rendered = tui_markdown::from_str_with_options(text, &options);
    let mut out = Vec::new();
    for line in rendered.lines {
        // Lines that already fit are passed through verbatim so tables keep
        // their column alignment and code keeps its exact spacing.
        if line.width() <= width as usize {
            out.push(prefix_line(own_line(line), "  "));
        } else {
            for wrapped in wrap_line(&line, width as usize) {
                out.push(prefix_line(wrapped, "  "));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from("  "));
    }
    out
}

/// Prepend `prefix` to a line without disturbing its spans or base style.
fn prefix_line(line: Line<'static>, prefix: &'static str) -> Line<'static> {
    let style = line.style;
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix));
    spans.extend(line.spans);
    Line::from(spans).style(style)
}

/// Convert a line borrowing from the Markdown source into an owned line.
fn own_line(line: Line<'_>) -> Line<'static> {
    let style = line.style;
    let spans = line
        .spans
        .into_iter()
        .map(|span| Span::styled(span.content.into_owned(), span.style))
        .collect::<Vec<_>>();
    Line::from(spans).style(style)
}

/// Wrap a styled line to `width` columns, preserving per-span styles.
///
/// Markdown output keeps source line breaks but does not wrap paragraphs, so
/// the view wraps them itself to keep scroll offsets in logical lines.
/// Continuation lines reuse the original line's indentation.
fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        // The line's own style (e.g. a heading) is a base that spans patch;
        // flatten it into every cell so rebuilt lines keep it.
        let style = line.style.patch(span.style);
        for c in span.content.chars() {
            cells.push((c, style));
        }
    }
    if cells.is_empty() {
        return vec![Line::from("")];
    }

    let indent = cells
        .iter()
        .take_while(|(c, _)| *c == ' ')
        .count()
        .min(width.saturating_sub(1));
    let indent_style = cells[0].1;

    // Split into words; runs of spaces are separators (collapsed on wrap).
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut word: Vec<(char, Style)> = Vec::new();
    for (c, style) in cells {
        if c == ' ' {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push((c, style));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.is_empty() {
        return vec![Line::from("")];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let start_line = |indent: usize, style: Style| {
        let mut line: Vec<(char, Style)> = Vec::new();
        for _ in 0..indent {
            line.push((' ', style));
        }
        (line, indent)
    };
    let (mut current, mut current_len) = start_line(indent, indent_style);

    for word in words {
        let word_len = word.len();
        let separator = usize::from(current_len > indent);
        if current_len + separator + word_len <= width {
            if separator == 1 {
                current.push((' ', word[0].1));
                current_len += 1;
            }
            current.extend(word);
            current_len += word_len;
            continue;
        }

        // Flush the current line if it holds more than indentation.
        if current_len > indent {
            out.push(to_line(&current));
            (current, current_len) = start_line(indent, indent_style);
        }

        // Append the word, hard-breaking anything longer than the width.
        let mut rest = &word[..];
        loop {
            let available = width.saturating_sub(current_len);
            if rest.len() <= available {
                current.extend_from_slice(rest);
                current_len += rest.len();
                break;
            }
            current.extend_from_slice(&rest[..available]);
            out.push(to_line(&current));
            (current, current_len) = start_line(indent, indent_style);
            rest = &rest[available..];
        }
    }
    if current_len > 0 {
        out.push(to_line(&current));
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// Collapse styled characters into spans, merging equal adjacent styles.
fn to_line(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    for (c, cell_style) in cells {
        if text.is_empty() {
            style = *cell_style;
        } else if *cell_style != style {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            style = *cell_style;
        }
        text.push(*c);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Greedy word wrap for plain text (no styling inside the body).
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut rest = paragraph;
        while !rest.is_empty() {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let (word, remainder) = rest.split_at(end);
            let remainder = remainder.trim_start();

            // Hard-break words longer than the width.
            if word.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let mut chunk: &str = word;
                while chunk.chars().count() > width {
                    let take = chunk
                        .char_indices()
                        .nth(width)
                        .map_or(chunk.len(), |(i, _)| i);
                    out.push(chunk[..take].to_string());
                    chunk = &chunk[take..];
                }
                line.push_str(chunk);
                rest = remainder;
                continue;
            }

            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
            rest = remainder;
        }
        if !line.is_empty() || out.is_empty() {
            out.push(line);
        }
    }
    out
}

/// `812` → `812`, `34_567` → `35k`.
fn format_k(tokens: usize) -> String {
    if tokens >= 1000 {
        format!("{}k", (tokens + 500) / 1000)
    } else {
        tokens.to_string()
    }
}

/// The bottom status line: phase/model/session/scroll on the left, the
/// context usage (`34k (27%)`) right-aligned, key hints between them.
fn status_line(app: &App, width: u16) -> Line<'static> {
    let phase = match app.model.phase {
        Phase::Idle => Span::styled("idle", Style::new().dim()),
        Phase::Busy => Span::styled("thinking…", Style::new().cyan()),
    };
    let session = match &app.model.session_id {
        Some(id) => {
            let short: String = id.chars().take(8).collect();
            Span::styled(format!(" session {short}"), Style::new().dim())
        }
        None => Span::raw(""),
    };
    let model = match &app.model_label {
        Some(label) => Span::styled(format!("  {label}"), Style::new().dim()),
        None => Span::raw(""),
    };
    let keys = match app.model.phase {
        Phase::Busy => Span::styled("  Esc interrupt", Style::new().dim()),
        Phase::Idle => Span::styled("  Enter send  Ctrl+T tab  Ctrl+D quit", Style::new().dim()),
    };
    // The scroll hint precedes the key hints so it stays visible on narrow
    // terminals (the status line is clipped, not wrapped).
    let scroll_hint = if app.follow {
        Span::raw("")
    } else {
        Span::styled(format!("  ↑{} more", app.scroll), Style::new().yellow())
    };
    let mut spans = vec![phase, model, session, scroll_hint];
    let width = width as usize;
    let len = |spans: &[Span]| {
        spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
    };

    if let Some((used, limit)) = app.context.filter(|(used, limit)| *used > 0 && *limit > 0) {
        let pct = used * 100 / limit;
        // Nearing the window means compaction is imminent; flag it.
        let style = if pct >= 80 {
            Style::new().yellow()
        } else {
            Style::new().dim()
        };
        let usage = format!("{} ({}%)", format_k(used), pct);
        // Drop the key hints (least critical) rather than crowd the usage
        // segment off a narrow terminal.
        if len(&spans) + keys.content.chars().count() + usage.chars().count() + 2 <= width {
            spans.push(keys);
        }
        let pad = width.saturating_sub(len(&spans) + usage.chars().count());
        spans.push(Span::raw(" ".repeat(pad.max(2))));
        spans.push(Span::styled(usage, style));
    } else {
        spans.push(keys);
    }
    Line::from(spans)
}

/// Draw the whole UI for one frame.
/// Full-workspace draw: tab bar on top, then the active tab's chat view.
pub fn draw(frame: &mut Frame, ws: &mut Workspace) {
    let root = frame.area();
    let [tab_area, chat] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(root);
    let [transcript_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(chat);

    let metas: Vec<TabMeta> = ws
        .tabs
        .iter()
        .map(|tab| {
            let mut label = tab_title(&tab.app);
            if tab.app.model.phase == Phase::Busy {
                label.push_str(" …");
            }
            TabMeta { id: tab.id, label }
        })
        .collect();
    ws.strip = layout_strip(&metas, ws.active, tab_area.width);
    render_strip(frame, &ws.strip, tab_area);

    let app = &mut ws.active_tab().app;
    render_chat(frame, app, transcript_area, input_area, status_area);
}

/// Single-conversation draw without the tab bar (used by the unit tests).
#[cfg(test)]
pub fn draw_app(frame: &mut Frame, app: &mut App) {
    let root = frame.area();
    let [transcript_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(root);
    render_chat(frame, app, transcript_area, input_area, status_area);
}

/// The one-row tab bar: `… label × │ label × │ … │ +`. The active tab's
/// label is cyan, bold, and underlined; everything else is dim, with a cyan
/// `…` marking tabs whose ask is still streaming. Hit regions live in the
/// strip (see [`crate::workspace`]).
fn render_strip(frame: &mut Frame, strip: &TabStrip, area: Rect) {
    let dim = Style::new().dim();
    let accent = Style::new().cyan().bold();
    let mut spans: Vec<Span> = Vec::new();
    let (lo, hi) = strip.window;
    if !strip.metas.is_empty() {
        if lo > 0 {
            spans.push(Span::styled("…", dim));
        }
        for (index, meta) in strip.metas.iter().enumerate().take(hi + 1).skip(lo) {
            let active = index == strip.active;
            // A busy tab's trailing " …" is styled separately, so a running
            // background tab stays noticeable even while dimmed. Splitting
            // keeps the label's column width identical either way.
            let (label, busy) = match meta.label.strip_suffix(" …") {
                Some(base) => (base, true),
                None => (meta.label.as_str(), false),
            };
            let label_style = if active {
                Style::new().cyan().bold().underlined()
            } else {
                dim
            };
            spans.push(Span::styled(format!(" {label} "), label_style));
            if busy {
                spans.push(Span::styled("… ", Style::new().cyan()));
            }
            spans.push(Span::styled("× ", if active { accent } else { dim }));
            spans.push(Span::styled(SEPARATOR, dim));
        }
        if hi + 1 < strip.metas.len() {
            spans.push(Span::styled("…", dim));
        }
    }
    // OpenCode-style: the `+` button sits directly after the last rendered
    // tab (or the right overflow marker), not pinned to the terminal edge.
    spans.push(Span::styled(NEW_BUTTON, Style::new().dim().bold()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Transcript, composer, and status line for one conversation.
fn render_chat(
    frame: &mut Frame,
    app: &mut App,
    transcript_area: Rect,
    input_area: Rect,
    status_area: Rect,
) {
    // -- transcript ----------------------------------------------------------
    let width = transcript_area.width;
    let phase = app.model.phase;
    let mut transcript = Lines::new(width);
    {
        // Split borrows so the render cache can be written while iterating.
        let App {
            model,
            rendered,
            expanded_groups,
            expanded_outputs,
            ..
        } = app;
        if rendered.len() > model.blocks.len() {
            rendered.clear();
        }
        rendered.resize_with(model.blocks.len(), || None);

        let busy = phase == Phase::Busy;
        let last = model.blocks.len().saturating_sub(1);
        let mut index = 0;
        while index < model.blocks.len() {
            if matches!(model.blocks[index], Msg::Tool { .. }) {
                let run_len = model.blocks[index..]
                    .iter()
                    .take_while(|block| matches!(block, Msg::Tool { .. }))
                    .count();
                tool_run(
                    &mut transcript,
                    &model.blocks[index..index + run_len],
                    index,
                    expanded_groups.contains(&index),
                    expanded_outputs,
                );
                index += run_len;
            } else {
                let streaming = busy && index == last;
                block_lines(
                    &model.blocks[index],
                    width,
                    streaming,
                    index,
                    rendered,
                    &mut transcript,
                );
                index += 1;
            }
        }
    }
    if transcript.lines.is_empty() {
        transcript.push(
            Line::from(Span::styled(
                "connected — type a message and press Enter",
                Style::new().dim(),
            )),
            None,
        );
    }
    let Lines { lines, clicks, .. } = transcript;

    let total = lines.len();
    let visible = transcript_area.height.saturating_sub(2) as usize; // borders
    let hidden = total.saturating_sub(visible);
    // A toggling click pins the clicked line to the same screen row: expansion
    // only inserts lines below it, so its absolute index is still valid.
    if let Some((line, row)) = app.anchor.take() {
        let top = line.saturating_sub(row as usize);
        app.scroll = hidden.saturating_sub(top);
        if app.scroll == 0 {
            app.follow = true;
        }
    }
    // `scroll_to_top` uses a sentinel; clamp it to the real hidden-line count
    // so the status indicator shows a meaningful number. A transcript that
    // fits entirely on screen has nothing to pin, so resume following.
    if !app.follow {
        app.scroll = app.scroll.min(hidden);
        if app.scroll == 0 {
            app.follow = true;
        }
    }
    let offset = if app.follow {
        hidden
    } else {
        hidden.saturating_sub(app.scroll)
    };
    let show: Vec<Line<'static>> = lines.into_iter().skip(offset).take(visible).collect();

    // Remember where the transcript landed so a click can be mapped back to
    // the line (and therefore the tool) under the pointer.
    app.transcript_area = transcript_area;
    app.viewport_top = offset;
    app.click_map = clicks;

    let transcript = Paragraph::new(show).block(Block::default().borders(Borders::ALL).title(
        Span::styled(" gewu ", Style::new().bold().add_modifier(Modifier::DIM)),
    ));
    frame.render_widget(transcript, transcript_area);

    // -- input ---------------------------------------------------------------
    let input_title = match app.model.phase {
        Phase::Idle => " prompt ",
        Phase::Busy => " prompt (busy — Esc interrupts) ",
    };
    let input =
        Paragraph::new(app.input.as_str()).block(Block::default().borders(Borders::ALL).title(
            Span::styled(input_title, Style::new().bold().add_modifier(Modifier::DIM)),
        ));
    frame.render_widget(input, input_area);

    // Place the cursor inside the input box on the first line.
    if app.model.phase == Phase::Idle {
        let first_line = app.input.lines().next().unwrap_or("");
        let cursor_col = app.cursor.min(first_line.chars().count());
        let x = (input_area.x + 1 + cursor_col as u16)
            .min(input_area.x + input_area.width.saturating_sub(2));
        frame.set_cursor_position((x, input_area.y + 1));
    }

    // -- status --------------------------------------------------------------
    frame.render_widget(
        Paragraph::new(status_line(app, status_area.width)),
        status_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TabClick;
    use gewu_protocol::{ContentPart, Message};
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_app(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_ws(ws: &mut Workspace, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, ws)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tab_row(screen: &str) -> &str {
        screen.lines().next().unwrap_or("")
    }

    #[test]
    fn tab_bar_shows_labels_close_marks_and_plus() {
        let mut ws = Workspace::new();
        ws.open_session(
            "aaaaaaaa1111",
            &[Message::User {
                content: vec![ContentPart::Text {
                    text: "first chat".to_string(),
                }],
            }],
        );
        ws.open_session("bbbbbbbb2222", &[]);

        let screen = render_ws(&mut ws, 80, 12);
        let row = tab_row(&screen);
        assert!(row.contains("first chat"), "label from first prompt: {row}");
        assert!(row.contains("bbbbbb…"), "short-id fallback: {row}");
        assert!(row.matches('×').count() == 2, "one × per tab: {row}");
        assert_eq!(row.matches('│').count(), 2, "one │ rule per tab: {row}");
        // Tabs are 14 + 1 + 13 + 1 columns; `+` follows the last rule.
        let plus_col = row.chars().position(|c| c == '+');
        assert_eq!(plus_col, Some(30), "+ after the last tab: {row:?}");
        assert!(row.trim_end().ends_with('+'), "no right-edge pad: {row:?}");
    }

    #[test]
    fn active_tab_is_highlighted() {
        let mut ws = Workspace::new();
        ws.open_session("aaaaaaaa1111", &[]);
        ws.open_session("bbbbbbbb2222", &[]);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut ws)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // The active tab's label is cyan, bold, and underlined (no reversed
        // block); the inactive tab is plain dim.
        let cyan = (0..80)
            .filter(|&x| buffer[(x, 0)].fg == Color::Cyan)
            .count();
        assert!(cyan > 0, "the active tab is cyan-accented");
        let underlined = (0..80)
            .filter(|&x| {
                buffer[(x, 0)]
                    .modifier
                    .contains(ratatui::style::Modifier::UNDERLINED)
            })
            .count();
        assert!(underlined > 0, "the active tab label is underlined");
        let reversed = (0..80)
            .filter(|&x| {
                buffer[(x, 0)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .count();
        assert_eq!(reversed, 0, "no inverted blocks: {reversed}");
    }

    #[test]
    fn busy_marker_is_cyan_even_in_background_tabs() {
        let mut ws = Workspace::new();
        ws.open_session(
            "aaaaaaaa1111",
            &[Message::User {
                content: vec![ContentPart::Text {
                    text: "hello".to_string(),
                }],
            }],
        );
        ws.open_session("bbbbbbbb2222", &[]);
        // The first tab streams in the background while the second is focused.
        ws.tabs[0].app.model.phase = Phase::Busy;

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut ws)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Row: ` hello … × │ bbbbbbbb… × │ + `. The label's ellipsis is the
        // first one on the row ("hello" is not truncated).
        let marker = (0..80)
            .find(|&x| buffer[(x, 0)].symbol() == "…")
            .expect("busy marker on the bar");
        assert_eq!(buffer[(marker, 0)].fg, Color::Cyan, "busy … is cyan");
        // The busy tab itself stays dim (it is not the focused one).
        assert!(
            buffer[(2, 0)]
                .modifier
                .contains(ratatui::style::Modifier::DIM),
            "background label is dim"
        );
    }

    #[test]
    fn tab_bar_clicks_map_through_the_drawn_strip() {
        let mut ws = Workspace::new();
        ws.open_session("aaaaaaaa1111", &[]);
        ws.open_blank();

        render_ws(&mut ws, 80, 12);
        // Draw stored the hit regions; simulate the event loop's routing.
        match ws.click(2, 0) {
            TabClick::Switch(0) => ws.switch_to(0),
            other => panic!("expected a tab switch, got {other:?}"),
        }
        assert_eq!(ws.active, 0);

        // `+` sits right after the two tabs (13 + 7 cols incl. rules).
        match ws.click(23, 0) {
            TabClick::New => {
                ws.open_blank();
            }
            other => panic!("expected the + button, got {other:?}"),
        }
        assert_eq!(ws.tabs.len(), 3);

        // The close hit carries the tab's id, for delete-and-close. The first
        // tab's label is 9 chars, so its `×` sits at columns 11-12.
        let id = match ws.click(11, 0) {
            TabClick::Close(id) => id,
            other => panic!("expected a close click, got {other:?}"),
        };
        assert!(ws.tab(id).is_some());
    }

    #[test]
    fn draws_transcript_input_status() {
        let mut app = App::new();
        app.model.blocks.push(Msg::User("hi there".into()));
        app.model.blocks.push(Msg::Assistant("hello!".into()));

        let screen = render(&mut app, 40, 12);
        assert!(screen.contains("gewu"), "title: {screen}");
        assert!(screen.contains("you"), "user label: {screen}");
        assert!(screen.contains("hi there"));
        assert!(screen.contains("hello!"));
        assert!(screen.contains("prompt"), "input box: {screen}");
        assert!(screen.contains("Enter send"), "status: {screen}");
    }

    #[test]
    fn status_line_shows_provider_and_model() {
        let mut app = App::new();
        app.model_label = Some("openai/gpt-4o".to_string());
        let screen = render(&mut app, 70, 10);
        assert!(screen.contains("openai/gpt-4o"), "status line: {screen}");

        // Unknown until the server reports it.
        let mut app = App::new();
        let screen = render(&mut app, 70, 10);
        assert!(!screen.contains("openai/"), "no label: {screen}");
    }

    #[test]
    fn status_line_shows_context_usage_right_aligned() {
        let mut app = App::new();
        app.context = Some((34_000, 128_000));
        let screen = render(&mut app, 80, 10);
        assert!(screen.contains("34k (26%)"), "usage segment: {screen}");
        let row = screen
            .lines()
            .find(|line| line.contains("34k"))
            .expect("usage row");
        assert!(
            row.trim_end().ends_with("34k (26%)"),
            "usage is right-aligned: {row:?}"
        );

        // Hidden until the server reports usage.
        let mut app = App::new();
        let screen = render(&mut app, 80, 10);
        assert!(!screen.contains('%'), "no usage segment: {screen}");
    }

    #[test]
    fn status_line_usage_wins_over_key_hints_when_crowded() {
        // Mimics an 80-column terminal with a long model + session label.
        let mut app = App::new();
        app.model_label = Some("openai/smoke-model".to_string());
        app.set_session(Some("seeded-session".to_string()));
        app.context = Some((40, 128_000));
        let screen = render(&mut app, 80, 10);
        assert!(screen.contains("(0%)"), "usage stays visible: {screen}");
        assert!(
            !screen.contains("Enter send"),
            "key hints yield to the usage: {screen}"
        );

        // With room to spare the key hints stay.
        let mut app = App::new();
        app.context = Some((40, 128_000));
        let screen = render(&mut app, 120, 10);
        assert!(screen.contains("Enter send"), "key hints kept: {screen}");
        assert!(screen.contains("(0%)"), "usage right-aligned: {screen}");
    }

    #[test]
    fn status_line_usage_turns_yellow_near_the_limit() {
        // 110k of 128k = 85%: compaction is imminent.
        let mut app = App::new();
        app.context = Some((110_000, 128_000));
        let cell = cell_at(&mut app, "(85%)");
        assert_eq!(cell.fg, Color::Yellow, "warning color: {cell:?}");

        // Comfortably inside the window: dim like the rest of the line.
        let mut app = App::new();
        app.context = Some((34_000, 128_000));
        let cell = cell_at(&mut app, "(26%)");
        assert_eq!(cell.fg, Color::Reset, "default dim segment: {cell:?}");
    }

    #[test]
    fn busy_phase_shows_interrupt_hint() {
        let mut app = App::new();
        app.submit("go".into());
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("thinking"));
        assert!(screen.contains("Esc interrupt"));
    }

    #[test]
    fn scroll_up_pins_history() {
        let mut app = App::new();
        for i in 0..20 {
            app.model.blocks.push(Msg::Assistant(format!("line {i}")));
        }
        app.scroll = 200;
        app.follow = false;
        let screen = render(&mut app, 30, 8);
        assert!(
            screen.contains("line 0"),
            "top visible when scrolled: {screen}"
        );
        assert!(screen.contains("more"), "scroll hint: {screen}");
    }

    #[test]
    fn scroll_to_top_clamps_and_shows_counts() {
        let mut app = App::new();
        for i in 0..20 {
            app.model.blocks.push(Msg::Assistant(format!("line {i}")));
        }
        app.scroll_to_top();
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("line 0"), "top visible: {screen}");
        assert!(screen.contains("more"), "hint shown: {screen}");
        // The sentinel must be clamped to the real hidden-line count.
        assert!(
            !screen.contains(&usize::MAX.to_string()),
            "unclamped sentinel leaked into status: {screen}"
        );
        assert!(app.scroll < usize::MAX, "scroll was clamped");

        app.scroll_to_bottom();
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("line 19"), "bottom visible: {screen}");
        assert!(
            !screen.contains("more"),
            "no hint while following: {screen}"
        );
    }

    #[test]
    fn scroll_to_top_with_short_transcript_resumes_follow() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Assistant("only line".into()));
        app.scroll_to_top();
        let _ = render(&mut app, 40, 20);
        assert!(app.follow, "nothing to pin in a short transcript");
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn tool_block_renders_command_without_label() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "ls -la".into(),
            output: Some("total 0".into()),
        });
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("bash  ls -la"), "command row: {screen}");
        assert!(screen.contains("total 0"), "output: {screen}");
        assert!(
            !screen.contains("tool"),
            "tool calls should not carry a speaker label: {screen}"
        );
    }

    #[test]
    fn tool_rows_use_readable_names() {
        assert_eq!(tool_row("bash", "ls -la"), ("bash", "ls -la".to_string()));
        assert_eq!(
            tool_row("read_file", "src/a.rs"),
            ("read", "src/a.rs".to_string())
        );
        assert_eq!(
            tool_row("write_file", "src/a.rs"),
            ("write", "src/a.rs".to_string())
        );
        assert_eq!(tool_row("edit_file", "src/a.rs").0, "edit");
        assert_eq!(tool_row("apply_patch", "p.diff").0, "patch");
        assert_eq!(tool_row("grep", "TODO").0, "grep");
        assert_eq!(tool_row("glob", "*.rs").0, "glob");
        assert_eq!(tool_row("list", "src").0, "list");
        assert_eq!(tool_row("webfetch", "https://x.test").0, "fetch");
        // Unknown tools keep their own name.
        assert_eq!(
            tool_row("custom_tool", "{\"a\":1}"),
            ("custom_tool", "{\"a\":1}".to_string())
        );
        // A history-loaded call without input falls back to the tool name.
        assert_eq!(tool_row("bash", ""), ("bash", "bash".to_string()));
    }

    #[test]
    fn tool_name_is_dim_and_title_is_bright() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "git status".into(),
            output: Some("ok".into()),
        });
        let name = modifier_of_row(&mut app, "bash");
        assert!(
            name.contains(Modifier::DIM),
            "tool name should be dim, got {name:?}"
        );
        let title = modifier_of_row(&mut app, "git status");
        assert!(
            !title.contains(Modifier::DIM),
            "the command should be default color, got {title:?}"
        );
    }

    #[test]
    fn running_tool_shows_trailing_ellipsis() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "sleep 1".into(),
            output: None,
        });
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("bash  sleep 1 …"), "{screen}");
    }

    #[test]
    fn tool_output_collapses_and_a_click_expands_it() {
        let mut app = App::new();
        let output = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "seq 10".into(),
            output: Some(output),
        });

        let screen = render(&mut app, 50, 20);
        assert!(screen.contains("line 6"), "preview lines: {screen}");
        assert!(
            !screen.contains("line 10"),
            "collapsed tail hidden: {screen}"
        );
        assert!(screen.contains("… +4 lines"), "overflow hint: {screen}");

        let y = row_of(&mut app, 50, 20, "bash  seq 10");
        assert!(app.click_at(3, y), "click on the tool row toggles it");

        let screen = render(&mut app, 50, 20);
        assert!(screen.contains("line 10"), "expanded output: {screen}");
        assert!(!screen.contains("+4 lines"), "hint gone: {screen}");

        // Clicking again collapses.
        let y = row_of(&mut app, 50, 20, "bash  seq 10");
        assert!(app.click_at(3, y));
        let screen = render(&mut app, 50, 20);
        assert!(screen.contains("… +4 lines"), "collapsed again: {screen}");
    }

    #[test]
    fn consecutive_tools_group_into_a_summary_row() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a".into()),
        });
        app.model.blocks.push(Msg::Tool {
            tool: "read_file".into(),
            input: "src/main.rs".into(),
            output: Some("b".into()),
        });

        let screen = render(&mut app, 60, 16);
        assert!(
            screen.contains("+ Explored: 1 bash, 1 read"),
            "group summary: {screen}"
        );
        assert!(
            !screen.contains("src/main.rs"),
            "individual rows stay hidden: {screen}"
        );

        let y = row_of(&mut app, 60, 16, "Explored");
        assert!(app.click_at(3, y), "clicking the summary expands it");

        let screen = render(&mut app, 60, 16);
        assert!(
            screen.contains("− Explored: 1 bash, 1 read"),
            "expanded summary: {screen}"
        );
        assert!(screen.contains("src/main.rs"), "rows revealed: {screen}");
        assert!(screen.contains("bash  ls"), "rows revealed: {screen}");
    }

    #[test]
    fn running_group_says_exploring() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: None,
        });
        app.model.blocks.push(Msg::Tool {
            tool: "grep".into(),
            input: "TODO".into(),
            output: None,
        });
        let screen = render(&mut app, 60, 16);
        assert!(
            screen.contains("+ Exploring: 1 bash, 1 grep"),
            "in-progress summary: {screen}"
        );
    }

    #[test]
    fn clicks_outside_the_transcript_do_nothing() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a".into()),
        });
        let _ = render(&mut app, 40, 12);
        assert!(!app.click_at(0, 0), "transcript border is not clickable");
        assert!(!app.click_at(5, 11), "input/status area is not clickable");
    }

    /// Screen row containing `needle` after drawing at the given size.
    fn row_of(app: &mut App, width: u16, height: u16, needle: &str) -> u16 {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_app(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if row.contains(needle) {
                return y;
            }
        }
        panic!("row containing {needle:?} not found");
    }

    /// Modifier of the first character of `needle` on the row containing it.
    fn modifier_of_row(app: &mut App, needle: &str) -> Modifier {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_app(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if let Some(pos) = row.find(needle) {
                let x = row[..pos].chars().count() as u16;
                return buffer[(x, y)].modifier;
            }
        }
        panic!("row containing {needle:?} not found");
    }

    #[test]
    fn assistant_text_is_not_dimmed() {
        let mut app = App::new();
        app.model
            .blocks
            .push(Msg::Assistant("important answer".into()));
        let modifier = modifier_of_row(&mut app, "important answer");
        assert!(
            !modifier.contains(Modifier::DIM),
            "assistant body should be default color, got {modifier:?}"
        );
    }

    #[test]
    fn tool_output_stays_dim() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("supporting output".into()),
        });
        let modifier = modifier_of_row(&mut app, "supporting output");
        assert!(
            modifier.contains(Modifier::DIM),
            "tool output should stay dim, got {modifier:?}"
        );
    }

    #[test]
    fn info_notes_stay_dim() {
        let mut app = App::new();
        app.note("connected");
        let modifier = modifier_of_row(&mut app, "connected");
        assert!(
            modifier.contains(Modifier::DIM),
            "info notes should stay dim, got {modifier:?}"
        );
    }

    /// Render a single finished assistant block as Markdown.
    fn markdown_screen(markdown: &str, width: u16) -> (App, String) {
        let mut app = App::new();
        app.model.blocks.push(Msg::Assistant(markdown.into()));
        let screen = render(&mut app, width, 24);
        (app, screen)
    }

    /// The cell holding the first character of `needle`.
    fn cell_at(app: &mut App, needle: &str) -> ratatui::buffer::Cell {
        let backend = TestBackend::new(70, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_app(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if let Some(pos) = row.find(needle) {
                let x = row[..pos].chars().count() as u16;
                return buffer[(x, y)].clone();
            }
        }
        panic!("{needle:?} not on screen");
    }

    #[test]
    fn markdown_heading_has_no_markers_and_is_accented() {
        let (mut app, screen) = markdown_screen("# Release notes", 60);
        assert!(screen.contains("Release notes"), "{screen}");
        assert!(
            !screen.contains('#'),
            "heading markers must be concealed: {screen}"
        );
        let cell = cell_at(&mut app, "Release notes");
        assert_eq!(cell.fg, Color::Cyan, "heading accent: {screen}");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "heading should be bold, got {:?}",
            cell.modifier
        );
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "H1 should be underlined, got {:?}",
            cell.modifier
        );
    }

    #[test]
    fn markdown_h2_is_bold_but_not_underlined() {
        let (mut app, screen) = markdown_screen("## Second level", 60);
        assert!(!screen.contains('#'), "markers concealed: {screen}");
        let cell = cell_at(&mut app, "Second level");
        assert_eq!(cell.fg, Color::Cyan, "heading accent: {screen}");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "H2 should be bold, got {:?}",
            cell.modifier
        );
        assert!(
            !cell.modifier.contains(Modifier::UNDERLINED),
            "H2 should not be underlined, got {:?}",
            cell.modifier
        );
    }

    #[test]
    fn markdown_inline_code_is_styled_without_backticks() {
        let (mut app, screen) = markdown_screen("Run `cargo test` now", 60);
        assert!(screen.contains("cargo test"), "{screen}");
        assert!(
            !screen.contains("`cargo test`"),
            "backticks should be stripped: {screen}"
        );
        let cell = cell_at(&mut app, "cargo test");
        assert_eq!(cell.fg, Color::Cyan, "inline code color: {screen}");
    }

    #[test]
    fn markdown_fenced_code_preserves_content() {
        let (_app, screen) = markdown_screen("```rust\nfn main() { println!(\"hi\"); }\n```", 70);
        assert!(screen.contains("```rust"), "fence: {screen}");
        assert!(screen.contains("fn main()"), "code body: {screen}");
        assert!(screen.contains("println!"), "code body: {screen}");
    }

    #[test]
    fn markdown_lists_render_bullets() {
        let (_app, screen) = markdown_screen("- alpha\n- beta", 50);
        assert!(screen.contains("- alpha"), "{screen}");
        assert!(screen.contains("- beta"), "{screen}");
    }

    #[test]
    fn markdown_paragraph_wraps_to_width() {
        let text = "The quick brown fox jumps over the lazy dog and keeps on running far away.";
        let lines = render_markdown(text, 20);
        assert!(lines.len() > 1, "paragraph should wrap: {lines:?}");
        for line in &lines {
            assert!(
                line.width() <= 22,
                "wrapped line exceeds body+indent width: {line:?}"
            );
        }
        let joined: String = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            joined.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>(),
            "wrapping must preserve every word"
        );
    }

    #[test]
    fn markdown_table_fits_pane_width() {
        let md = "| Name | Value |\n| --- | --- |\n| alpha | a fairly long cell value |";
        let lines = render_markdown(md, 30);
        for line in &lines {
            assert!(
                line.width() <= 32,
                "table line too wide ({}): {line:?}",
                line.width()
            );
        }
    }

    #[test]
    fn markdown_table_columns_stay_aligned() {
        let md = "| Name | Value |\n| --- | --- |\n| alpha | beta |";
        let lines = render_markdown(md, 30);
        let rows: Vec<&Line<'static>> = lines.iter().filter(|line| line.width() > 2).collect();
        assert!(rows.len() >= 5, "expected a bordered table, got {lines:?}");
        let width = rows[0].width();
        assert!(
            rows.iter().all(|row| row.width() == width),
            "table rows must stay aligned: {rows:?}"
        );
    }

    #[test]
    fn markdown_code_block_keeps_indentation() {
        let lines = render_markdown("```\n    indented\n```", 40);
        let texts: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(
            texts.iter().any(|text| text == "      indented"),
            "code indentation lost: {texts:?}"
        );
    }

    #[test]
    fn streaming_assistant_renders_raw_then_markdown() {
        let mut app = App::new();
        app.submit("hi".into());
        app.model.blocks.push(Msg::Assistant("**bold**".into()));

        let raw = render(&mut app, 60, 12);
        assert!(raw.contains("**bold**"), "raw while streaming: {raw}");

        app.finish_ask();
        let done = render(&mut app, 60, 12);
        assert!(done.contains("bold"), "{done}");
        assert!(
            !done.contains("**bold**"),
            "finished reply should render as Markdown: {done}"
        );
    }

    #[test]
    fn rendered_markdown_is_cached_and_width_aware() {
        let mut app = App::new();
        app.model.blocks.push(Msg::Assistant("# cached".into()));

        let _ = render(&mut app, 50, 12);
        assert_eq!(app.rendered.len(), 1);
        assert_eq!(app.rendered[0].as_ref().unwrap().width, 47);

        // A resize re-renders at the new body width.
        let _ = render(&mut app, 60, 12);
        assert_eq!(app.rendered[0].as_ref().unwrap().width, 57);
    }

    #[test]
    fn wrap_text_handles_long_words_and_blank_lines() {
        assert_eq!(wrap_text("abc", 10), vec!["abc"]);
        assert_eq!(wrap_text("a b c", 3), vec!["a b", "c"]);
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("x\n\ny", 10), vec!["x", "", "y"]);
        assert_eq!(wrap_text("", 5), vec![""]);
    }

    #[test]
    fn empty_state_shows_hint() {
        let mut app = App::new();
        let screen = render(&mut app, 40, 10);
        assert!(screen.contains("connected"));
    }
}
