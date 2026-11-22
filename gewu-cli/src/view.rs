//! Single-session rendering adapted from the original gewu-tui.

use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use tui_markdown::{Options, StyleSheet};

use crate::chat::Chat;
use gewu_client::{Block as Msg, Phase, SessionModel};
use ratatui_textarea::{TextArea, WrapMode};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy)]
enum Toggle {
    Group(usize),
    Output(usize),
}

#[derive(Clone)]
struct RenderedBlock {
    hash: u64,
    width: u16,
    lines: Vec<Line<'static>>,
}

#[derive(Default)]
pub(crate) struct View {
    rendered: Vec<Option<RenderedBlock>>,
    expanded_groups: HashSet<usize>,
    expanded_outputs: HashSet<usize>,
    click_map: Vec<Option<Toggle>>,
    transcript_area: Rect,
    viewport_top: usize,
    pub scroll: usize,
    pub follow_choice: bool,
}

pub(crate) fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default());
    editor
}

/// Position the terminal's native caret after layout so input methods follow the
/// same wrapped rows, character widths and scrolling as the editor.
pub(crate) fn place_caret(frame: &mut Frame, editor: &TextArea<'_>, area: Rect) {
    if area.is_empty() {
        return;
    }
    let cursor = editor.screen_cursor();
    let (top, left) = editor.scroll_offset();
    let x = cursor
        .col
        .saturating_sub(usize::from(left))
        .min(usize::from(area.width - 1));
    let y = cursor
        .row
        .saturating_sub(usize::from(top))
        .min(usize::from(area.height - 1));
    frame.set_cursor_position((area.x + x as u16, area.y + y as u16));
}

/// Defensive cap on expanded tool output (the session model already trims).
const TOOL_MAX_LINES: usize = 500;

/// Colors for each transcript speaker.
fn label_style(label: &str) -> Style {
    match label {
        "you" => Style::new().bold(),
        "gewu" => Style::new().dim(),
        "error" => Style::new().red().bold(),
        _ => Style::new().dim(),
    }
}

/// Markdown styles aligned with the TUI palette: important text keeps the
/// terminal's default foreground, supporting detail is dimmed.
#[derive(Clone, Copy, Debug)]
struct GewuStyles;

impl StyleSheet for GewuStyles {
    fn heading(&self, _level: u8) -> Style {
        Style::new().bold()
    }

    fn heading_marker(&self, _level: u8) -> &str {
        // Conceal `#`/`##` markers; the styled text carries the heading.
        ""
    }

    fn code(&self) -> Style {
        Style::new().fg(Color::Rgb(155, 215, 149))
    }

    fn link(&self) -> Style {
        Style::new().blue().underlined()
    }

    fn blockquote(&self) -> Style {
        Style::new().dim()
    }

    fn list_marker(&self) -> Style {
        Style::new().fg(Color::Rgb(135, 218, 209))
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
    if matches!(block, Msg::User(_) | Msg::UserMedia { .. }) {
        let band = Style::new().bg(Color::Rgb(64, 69, 79));
        let blank = Line::styled(" ".repeat(width.into()), band);
        out.push(blank.clone(), None);
        for (i, row) in wrap_text(block.text(), out.avail(2))
            .into_iter()
            .enumerate()
        {
            let tail = usize::from(width).saturating_sub(2 + row.width());
            out.push(
                Line::from(vec![
                    Span::styled(if i == 0 { "› " } else { "  " }, band.dim()),
                    Span::styled(row, band),
                    Span::styled(" ".repeat(tail), band),
                ]),
                None,
            );
        }
        out.push(blank, None);
        out.blank();
        return;
    }
    let label = block.label();
    let first = out.lines.len();

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
    if let Some(line) = out.lines.get_mut(first) {
        let marker = match label {
            "you" => "› ",
            "gewu" => "• ",
            "error" => "! ",
            _ if matches!(block, Msg::Info(s) if s.starts_with("Worked for ")) => "  ",
            _ => "· ",
        };
        if let Some(span) = line.spans.first_mut()
            && let Some(text) = span.content.strip_prefix("  ")
        {
            span.content = text.to_owned().into();
        }
        line.spans
            .insert(0, Span::styled(marker, label_style(label)));
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
        0
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
    let grouped = run.len() > 1;
    if grouped {
        let glyph = if group_expanded { "−" } else { "+" };
        out.wrapped(
            2,
            &format!("{glyph} {}", group_label(run)),
            Style::new().dim(),
            Some(Toggle::Group(first)),
        );
    }
    for (offset, block) in run.iter().enumerate() {
        let Msg::Tool {
            tool,
            input,
            output,
            status,
            ..
        } = block
        else {
            continue;
        };
        let index = first + offset;
        if !grouped || group_expanded {
            tool_block(
                out,
                index,
                tool,
                input,
                output.as_deref(),
                expanded_outputs.contains(&index),
                if grouped { 4 } else { 2 },
            );
        }
        if let Some(message) = status {
            let target = if grouped && !group_expanded {
                Toggle::Group(first)
            } else {
                Toggle::Output(index)
            };
            out.wrapped(4, message, Style::new().dim(), Some(target));
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
    for mut line in rendered.lines {
        if let Some(marker) = line.spans.first_mut()
            && marker.style == Style::default()
            && marker.content.trim_start_matches(' ') == "- "
        {
            marker.content = marker.content.replacen('-', "•", 1).into();
            marker.style = GewuStyles.list_marker();
        }
        for span in &mut line.spans {
            if span.style.add_modifier.contains(Modifier::BOLD) && span.style.fg.is_none() {
                span.style = span.style.fg(Color::Rgb(245, 136, 173));
            }
        }
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

/// Ratatui exposes styled graphemes; count terminal columns rather than bytes
/// or chars so a CJK character is never split between rows.
fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut columns = 0;
    for glyph in line.styled_graphemes(Style::default()) {
        let size = glyph.symbol.width();
        if columns > 0 && columns + size > width.max(1) {
            lines.push(Line::from(std::mem::take(&mut spans)));
            columns = 0;
        }
        spans.push(Span::styled(glyph.symbol.to_owned(), glyph.style));
        columns += size;
    }
    lines.push(Line::from(spans));
    lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    textwrap::wrap(text, width.max(1))
        .into_iter()
        .map(|s| s.into_owned())
        .collect()
}

impl View {
    pub fn click(&mut self, column: u16, row: u16) {
        let inner = self.transcript_area;
        if !inner.contains((column, row).into()) {
            return;
        }
        let line = self.viewport_top + usize::from(row - inner.y);
        if let Some(Some(target)) = self.click_map.get(line) {
            let (set, index) = match *target {
                Toggle::Group(i) => (&mut self.expanded_groups, i),
                Toggle::Output(i) => (&mut self.expanded_outputs, i),
            };
            if !set.remove(&index) {
                set.insert(index);
            }
        }
    }

    fn transcript(&mut self, model: &SessionModel, width: u16) -> Lines {
        let mut lines = Lines::new(width);
        self.rendered.resize_with(model.blocks.len(), || None);
        let mut i = 0;
        while i < model.blocks.len() {
            if matches!(model.blocks[i], Msg::Tool { .. }) {
                let count = model.blocks[i..]
                    .iter()
                    .take_while(|b| matches!(b, Msg::Tool { .. }))
                    .count();
                tool_run(
                    &mut lines,
                    &model.blocks[i..i + count],
                    i,
                    self.expanded_groups.contains(&i),
                    &self.expanded_outputs,
                );
                i += count;
            } else {
                let streaming = model.phase == Phase::Busy && i + 1 == model.blocks.len();
                block_lines(
                    &model.blocks[i],
                    width,
                    streaming,
                    i,
                    &mut self.rendered,
                    &mut lines,
                );
                i += 1;
            }
        }
        lines
    }
}

fn selection_style() -> Style {
    Style::new()
        .bg(Color::Rgb(96, 165, 250))
        .fg(Color::Rgb(12, 22, 38))
}

pub(crate) fn draw(frame: &mut Frame, chat: &mut Chat) {
    if let Some(entry) = &chat.key_entry {
        entry.draw(frame);
        return;
    }
    if let Some(designer) = &chat.designer {
        designer.draw(frame);
        return;
    }
    let area = frame.area().inner(Margin::new(1, 0));
    let panel = chat.panel_text();
    let empty = panel.is_none() && chat.model.blocks.is_empty();
    let suggestions = chat.suggestions();
    let width = usize::from(area.width.saturating_sub(3).max(1));
    let editor_rows = chat
        .editor
        .lines()
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>()
        .clamp(1, 6);
    let menu_rows = suggestions
        .len()
        .min(usize::from(area.height.saturating_sub(8)))
        .min(8);
    let working = panel.is_none() && chat.model.phase == Phase::Busy;
    let [header, body, activity, menu, input, summary, footer] = Layout::vertical([
        Constraint::Length(if empty && area.height >= 18 { 4 } else { 1 }),
        Constraint::Min(1),
        Constraint::Length(u16::from(working)),
        Constraint::Length(menu_rows as u16),
        Constraint::Length(editor_rows as u16 + 2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let directory = std::env::var_os("HOME")
        .and_then(|home| {
            std::path::Path::new(&chat.directory)
                .strip_prefix(home)
                .ok()
                .map(|rest| {
                    if rest.as_os_str().is_empty() {
                        "~".to_owned()
                    } else {
                        std::path::Path::new("~").join(rest).display().to_string()
                    }
                })
        })
        .unwrap_or_else(|| chat.workspace_label().to_owned());
    let dim = Style::new().dim();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(">_ ", Style::new().light_blue()),
                Span::styled("GeWu", Style::new().bold()),
                Span::styled(format!(" (v{})", env!("CARGO_PKG_VERSION")), dim),
            ]),
            Line::from(vec![
                Span::styled("   ", dim),
                Span::styled(&directory, dim),
            ]),
        ]),
        header.inner(Margin::new(2, if header.height == 4 { 1 } else { 0 })),
    );
    let mut selected_line = None;
    let mut lines = match &panel {
        None => chat.view.transcript(&chat.model, body.width),
        Some((text, choice)) => {
            let mut lines = Lines::new(body.width);
            lines.push(
                Line::styled(chat.mode_title().trim(), Style::new().bold()),
                None,
            );
            for (index, row) in text.lines().enumerate() {
                if Some(index) == *choice {
                    selected_line = Some(lines.lines.len());
                    for part in wrap_text(row, lines.avail(0)) {
                        lines.push(Line::styled(part, selection_style()), None);
                    }
                } else {
                    lines.wrapped(0, row, Style::default(), None);
                }
            }
            lines
        }
    };
    let height = usize::from(body.height);
    let hidden = lines.lines.len().saturating_sub(height);
    if std::mem::take(&mut chat.view.follow_choice)
        && let Some(row) = selected_line
    {
        chat.view.scroll = chat
            .view
            .scroll
            .clamp(row.saturating_sub(height.saturating_sub(1)), row);
    }
    chat.view.scroll = chat.view.scroll.min(hidden);
    let offset = if panel.is_some() {
        chat.view.scroll
    } else {
        hidden - chat.view.scroll
    };
    chat.view.transcript_area = body;
    chat.view.viewport_top = offset;
    chat.view.click_map = lines.clicks;
    let visible: Vec<_> = lines.lines.drain(..).skip(offset).take(height).collect();
    if empty && chat.editor.is_empty() && suggestions.is_empty() && body.height >= 5 {
        let mut guide = vec![
            Line::styled("Describe a task or try a command:", dim),
            Line::default(),
        ];
        for spec in crate::commands::COMMANDS.iter().take(7) {
            guide.push(Line::from(vec![
                Span::raw(spec.name),
                Span::styled(format!(" — {}", spec.help), dim),
            ]));
        }
        frame.render_widget(Paragraph::new(guide), body.inner(Margin::new(2, 0)));
    } else {
        frame.render_widget(Paragraph::new(visible), body);
    }
    if working {
        let elapsed = chat
            .turn_started
            .map(|t| format!("{}s · ", t.elapsed().as_secs()))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(format!("• Working ({elapsed}Ctrl+C to interrupt)")).style(dim),
            activity,
        );
    }
    let band = Style::new()
        .fg(Color::Rgb(235, 237, 242))
        .bg(Color::Rgb(58, 63, 75));
    // Keep the content inset, but paint the same background through both terminal
    // edges so inline IME preedit cannot push a contrasting gutter sideways.
    let background = Rect::new(frame.area().x, input.y, frame.area().width, input.height);
    frame.render_widget(Block::default().style(band), background);
    let composer = input.inner(Margin::new(0, 1));
    let [prompt, field] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(composer);
    frame.render_widget(Paragraph::new("›").style(Style::new().bold()), prompt);
    chat.editor.set_style(band);
    // Reserve the final column for the insertion point after a full-width line.
    let text_area = Rect {
        width: field.width.saturating_sub(1),
        ..field
    };
    if !text_area.is_empty() {
        frame.render_widget(&chat.editor, text_area);
    }
    // Terminal.app can shift the unoccupied tail of a row during inline IME
    // composition. Restore it even when the logical background did not change.
    for y in field.y..field.bottom() {
        for x in background.x..prompt.x {
            frame.buffer_mut()[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
        }
        for x in (field.x..background.right()).rev() {
            let cell = &mut frame.buffer_mut()[(x, y)];
            if cell.symbol() != " " {
                break;
            }
            cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }
    place_caret(frame, &chat.editor, field);
    let start = (chat.command_index + 1).saturating_sub(menu_rows);
    let selection = selection_style();
    let column = if menu.width >= 70 { 15 } else { 12 };
    for (row, (name, detail)) in suggestions.iter().skip(start).take(menu_rows).enumerate() {
        let selected = start + row == chat.command_index;
        let row_area = Rect::new(menu.x, menu.y + row as u16, menu.width, 1);
        let base = if selected {
            selection
        } else {
            Style::default()
        };
        let name_style = if selected { base.bold() } else { base };
        let detail_style = if selected { base } else { dim };
        let idle_only = chat.model.phase == Phase::Busy
            && crate::commands::lookup(name).is_some_and(|spec| spec.idle);
        let line = Line::from(vec![
            Span::styled(if selected { "› " } else { "  " }, name_style),
            Span::styled(format!("{name:<column$}"), name_style),
            Span::styled(
                if idle_only {
                    format!("{detail} · wait for turn")
                } else {
                    detail.to_string()
                },
                detail_style,
            ),
        ]);
        frame.render_widget(Paragraph::new(line).style(base), row_area);
    }
    let hint = if suggestions.is_empty() {
        chat.status()
    } else {
        ""
    };
    let context = chat
        .context
        .map(|(used, limit)| {
            format!(
                "~{}% context left",
                limit.saturating_sub(used).saturating_mul(100) / limit.max(1)
            )
        })
        .unwrap_or_default();
    let [badge, usage] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(context.len() as u16)])
            .areas(summary.inner(Margin::new(2, 0)));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(chat.model_badge(), Style::new().light_yellow()),
            Span::styled(format!(" · {}", chat.agent), dim),
            Span::styled(" · ", dim),
            Span::styled(&directory, Style::new().light_green()),
        ])),
        badge,
    );
    frame.render_widget(
        Paragraph::new(hint).style(dim),
        footer.inner(Margin::new(2, 0)),
    );
    frame.render_widget(Paragraph::new(context).style(dim), usage);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replies_distinguish_emphasis_code_and_list_markers() {
        let lines = render_markdown("**bold** and `code`\n\n- first", 80);
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        for (text, color) in [
            ("bold", Color::Rgb(245, 136, 173)),
            ("code", Color::Rgb(155, 215, 149)),
            ("• ", Color::Rgb(135, 218, 209)),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|s| s.content == text && s.style.fg == Some(color)),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn chinese_markdown_wraps_on_terminal_columns() {
        let rows = wrap_line(&Line::from("中文 hello"), 4);
        assert!(rows.iter().all(|line| line.width() <= 4));
        assert_eq!(rows[0].to_string(), "中文");
        assert_eq!(
            rows.iter().map(ToString::to_string).collect::<String>(),
            "中文 hello"
        );
    }
}
