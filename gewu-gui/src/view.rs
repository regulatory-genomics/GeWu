//! HTML rendering for the web GUI.
//!
//! Transcript blocks, Markdown, tool runs, and the status line. Everything
//! user- or model-authored is escaped, and raw HTML in Markdown is neutralized.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use gewu_client::session::{Block, Phase};
use gewu_protocol::{DirectoryEntry, FileEntry, ModelSummary, WorkspaceSummary};
use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

use crate::protocol::TabInfo;
use crate::workspace::Tab;

/// Lines of tool output shown before collapsing.
const TOOL_PREVIEW_LINES: usize = 6;
/// Defensive cap on expanded tool output. Tool output is already truncated
/// server-side; this only guards against pathological renders.
const TOOL_MAX_LINES: usize = 5000;
/// Commands longer than this (or spanning multiple lines) get a full-width
/// block when the tool is expanded, since the inline title ellipsizes.
const COMMAND_INLINE_LIMIT: usize = 80;

/// Escape text for use in HTML text or a double-quoted attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render the active tab's transcript.
pub fn transcript_html(tab: &Tab) -> String {
    if tab.model.blocks.is_empty() {
        return "<div class=\"empty\">connected — type a message and press Enter</div>".to_string();
    }

    let mut out = String::new();
    let last = tab.model.blocks.len() - 1;
    let mut index = 0;
    while index < tab.model.blocks.len() {
        if matches!(tab.model.blocks[index], Block::Tool { .. }) {
            let run_len = tab.model.blocks[index..]
                .iter()
                .take_while(|block| matches!(block, Block::Tool { .. }))
                .count();
            render_tool_run(
                &mut out,
                tab,
                &tab.model.blocks[index..index + run_len],
                index,
            );
            index += run_len;
        } else {
            let streaming = tab.busy() && index == last;
            render_block(&mut out, tab, &tab.model.blocks[index], index, streaming);
            index += 1;
        }
    }
    out
}

/// Render one non-tool transcript block.
fn render_block(out: &mut String, tab: &Tab, block: &Block, index: usize, streaming: bool) {
    if let Block::Compaction { summary } = block {
        let _ = writeln!(
            out,
            "<div class=\"block compaction\" data-block=\"{index}\"><details><summary>Context compacted</summary><div class=\"body markdown\">{}</div></details></div>",
            markdown_to_html(summary)
        );
        return;
    }
    let class = match block {
        Block::User(_) => "user",
        Block::Assistant(_) => "assistant",
        Block::Thought { .. } => "thought",
        Block::Error(_) => "error",
        Block::Info(_) => "info",
        Block::Compaction { .. } => unreachable!(),
        Block::Tool { .. } => return,
    };
    let _ = writeln!(out, "<div class=\"block {class}\" data-block=\"{index}\">");
    write_block_body(out, tab, block, index, streaming);
    if matches!(block, Block::User(_)) {
        let _ = write!(
            out,
            "<button type=\"button\" class=\"message-menu-trigger\" aria-label=\"Message actions\" aria-haspopup=\"menu\" aria-expanded=\"false\" aria-controls=\"message-menu-{index}\"></button><div id=\"message-menu-{index}\" class=\"message-menu\" role=\"menu\" hidden><button type=\"button\" role=\"menuitem\" class=\"chat-revert\" data-revert-block=\"{index}\" title=\"Revert conversation from here; files are not undone\">Revert chat</button></div>"
        );
    }
    out.push_str("</div>\n");
}

/// Render the inner `.body` div of one block (between the `.block` wrapper).
fn write_block_body(out: &mut String, tab: &Tab, block: &Block, index: usize, streaming: bool) {
    match block {
        // Streaming and final assistant bodies are both Markdown; streaming
        // only adds a class (the blinking cursor) so the text formats in
        // real time and settles into the identical form once done.
        Block::Assistant(text) => {
            let _ = writeln!(
                out,
                "<div class=\"body markdown{}\">{}</div>",
                if streaming { " streaming" } else { "" },
                markdown_to_html(text)
            );
        }
        Block::Thought { .. } => {
            out.push_str(&thought_body_html(tab, index, streaming));
        }
        _ => {
            let _ = writeln!(
                out,
                "<div class=\"body plain\">{}</div>",
                escape(block.text())
            );
        }
    }
}

/// Render a thought block's body: the "Thinking… Ns"/"Thought Ns" label with
/// a clickable `+`/`−` toggle when reasoning content exists, plus the escaped
/// reasoning text when expanded. Used by the full transcript, targeted block
/// renders, and streaming swaps so all three shapes stay identical.
fn thought_body_html(tab: &Tab, index: usize, streaming: bool) -> String {
    let Some(block @ Block::Thought { reasoning, .. }) = tab.model.blocks.get(index) else {
        return String::new();
    };
    let class = if streaming { " streaming" } else { "" };
    if reasoning.is_empty() {
        // Providers without reasoning content keep the plain indicator line.
        return format!(
            "<div class=\"body plain thought{class}\">{}</div>\n",
            escape(block.text())
        );
    }
    let expanded = tab.expanded_thoughts.contains(&index);
    let glyph = if expanded { "−" } else { "+" };
    let mut out = format!(
        "<div class=\"body plain thought{class}\">\
         <button type=\"button\" class=\"thought-toggle\" data-action=\"toggle_thought\" \
         data-tab=\"{}\" data-block=\"{index}\">{glyph} {}</button>",
        tab.id,
        escape(block.text())
    );
    if expanded {
        out.push_str(&format!(
            "<pre class=\"thought-body\">{}</pre>",
            escape(reasoning.trim_end())
        ));
    }
    out.push_str("</div>\n");
    out
}

/// Render one block as a standalone `.block` div, for targeted appends.
pub fn block_html(tab: &Tab, index: usize) -> Option<String> {
    let block = tab.model.blocks.get(index)?;
    if matches!(block, Block::Tool { .. }) {
        return None;
    }
    let streaming = tab.busy() && index == tab.model.blocks.len() - 1;
    let mut out = String::new();
    render_block(&mut out, tab, block, index, streaming);
    Some(out)
}

/// Render the body of a streaming block — assistant Markdown or the live
/// "Thinking…" indicator — for in-place swaps.
pub fn assistant_body_html(tab: &Tab, index: usize, streaming: bool) -> Option<String> {
    match tab.model.blocks.get(index) {
        Some(Block::Assistant(text)) => Some(format!(
            "<div class=\"body markdown{}\">{}</div>",
            if streaming { " streaming" } else { "" },
            markdown_to_html(text)
        )),
        Some(Block::Thought { .. }) => Some(thought_body_html(tab, index, streaming)),
        _ => None,
    }
}

/// Render just the tool card that starts at block `first`, for targeted
/// updates after a toggle. `None` when `first` is not a tool block.
pub fn tool_card_html(tab: &Tab, first: usize) -> Option<String> {
    if !matches!(tab.model.blocks.get(first), Some(Block::Tool { .. })) {
        return None;
    }
    let run_len = tab.model.blocks[first..]
        .iter()
        .take_while(|block| matches!(block, Block::Tool { .. }))
        .count();
    let mut out = String::new();
    render_tool_run(
        &mut out,
        tab,
        &tab.model.blocks[first..first + run_len],
        first,
    );
    Some(out)
}

/// Render a run of consecutive tool blocks inside one card.
fn render_tool_run(out: &mut String, tab: &Tab, run: &[Block], first: usize) {
    let tools: Vec<(&str, &str, Option<&str>)> = run
        .iter()
        .filter_map(|block| match block {
            Block::Tool {
                tool,
                input,
                output,
            } => Some((tool.as_str(), input.as_str(), output.as_deref())),
            _ => None,
        })
        .collect();

    let _ = writeln!(out, "<div class=\"tool-card\" data-card=\"{first}\">");
    if tools.len() == 1 {
        let (tool, input, output) = tools[0];
        render_tool(out, tab, tool, input, output, first, 0);
    } else {
        // Workflow runs default to expanded so their graphs stay visible,
        // but an explicit collapse (collapsed_groups) wins.
        let expanded = if is_workflow_run(run) {
            !tab.collapsed_groups.contains(&first)
        } else {
            tab.expanded_groups.contains(&first)
        };
        let glyph = if expanded { "−" } else { "+" };
        let running = run
            .iter()
            .any(|block| matches!(block, Block::Tool { output: None, .. }));
        let _ = writeln!(
            out,
            "<div class=\"tool-group{}\" data-block=\"{first}\" data-action=\"toggle_group\" data-tab=\"{}\">\
             <button class=\"tool-summary\" data-action=\"toggle_group\" data-tab=\"{}\" data-block=\"{first}\">{glyph} {}</button>",
            if running { " running" } else { "" },
            tab.id,
            tab.id,
            escape(&group_label(run))
        );
        if expanded {
            for (offset, (tool, input, output)) in tools.iter().enumerate() {
                render_tool(out, tab, tool, input, *output, first + offset, 1);
            }
        }
        out.push_str("</div>\n");
    }
    out.push_str("</div>\n");
}

/// Whether a tool run contains workflow calls. The view and the
/// `ToggleGroup` handler both use this so they classify runs identically.
pub(crate) fn is_workflow_run(run: &[Block]) -> bool {
    run.iter()
        .any(|block| matches!(block, Block::Tool { tool, .. } if tool.starts_with("workflow_")))
}

/// One tool row plus its output.
#[allow(clippy::too_many_arguments)]
fn render_tool(
    out: &mut String,
    tab: &Tab,
    tool: &str,
    input: &str,
    output: Option<&str>,
    index: usize,
    depth: usize,
) {
    let (name, title) = tool_row(tool, input);
    let running = output.is_none();
    let expanded = tab.expanded_outputs.contains(&index);
    let _ = writeln!(
        out,
        "<div class=\"tool{} depth-{depth}\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">\
         <button class=\"tool-row\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">\
         <span class=\"tool-name\">{}</span>{} <span class=\"tool-title\">{}</span></button>",
        if running { " running" } else { "" },
        tab.id,
        tab.id,
        escape(name),
        if running {
            "<span class=\"tool-spinner\" aria-hidden=\"true\"></span>"
        } else {
            ""
        },
        escape(&title)
    );

    // Workflow cards draw the run's graph between the row and the output.
    if let Some(graph) = crate::workflow::card_graph(tab, tool, input, output, index) {
        out.push_str(&graph);
    }

    let Some(output) = output else {
        out.push_str("</div>\n");
        return;
    };
    // The one-line title ellipsizes long commands, so when the tool is
    // expanded show the full command in its own block first.
    if expanded && (input.len() > COMMAND_INLINE_LIMIT || input.contains('\n')) {
        let _ = writeln!(
            out,
            "<pre class=\"tool-input\">{}</pre>",
            escape(input.trim_end())
        );
    }
    let lines: Vec<&str> = output.trim_end().lines().collect();
    if lines.is_empty() {
        out.push_str("</div>\n");
        return;
    }
    let shown = if expanded {
        lines.len().min(TOOL_MAX_LINES)
    } else {
        lines.len().min(TOOL_PREVIEW_LINES)
    };
    let _ = writeln!(
        out,
        "<pre class=\"tool-output\">{}</pre>",
        escape(&lines[..shown].join("\n"))
    );
    if lines.len() > shown {
        let hidden = lines.len() - shown;
        let _ = writeln!(
            out,
            "<button class=\"tool-more\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">… +{hidden} line{}</button>",
            tab.id,
            if hidden == 1 { "" } else { "s" }
        );
    }
    out.push_str("</div>\n");
}

/// Readable display name for a tool.
fn tool_name(tool: &str) -> &str {
    match tool {
        "read_file" | "read" => "read",
        "write_file" | "write" => "write",
        "edit_file" | "edit" => "edit",
        "apply_patch" | "patch" => "patch",
        "webfetch" | "web_fetch" => "fetch",
        "websearch" | "web_search" => "search",
        "shell" => "bash",
        "workflow_start" => "wf·start",
        "workflow_status" => "wf·status",
        "workflow_wait" => "wf·wait",
        "workflow_cancel" => "wf·cancel",
        other => other,
    }
}

fn tool_row<'a>(tool: &'a str, input: &str) -> (&'a str, String) {
    let title = match tool {
        "workflow_start" => crate::workflow::spec_title(input).unwrap_or_else(|| input.to_string()),
        // Status/wait/cancel inputs are already just the run_id.
        "workflow_status" | "workflow_wait" | "workflow_cancel" => input.to_string(),
        _ if input.is_empty() => tool.to_string(),
        _ => input.to_string(),
    };
    (tool_name(tool), title)
}

/// `Explored: 2 bash, 1 read` (or `Exploring: …` while a tool runs).
fn group_label(run: &[Block]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    let mut complete = true;
    for block in run {
        let Block::Tool { tool, output, .. } = block else {
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

fn plural(name: &str, count: usize) -> String {
    if count == 1 || name == "bash" {
        name.to_string()
    } else if name.ends_with('s') || name.ends_with("ch") || name.ends_with("sh") {
        format!("{name}es")
    } else {
        format!("{name}s")
    }
}

/// Marker around each extracted math segment's index; alphanumeric only, so
/// it flows through Markdown parsing untouched and is swapped for rendered
/// MathML afterwards.
const MATH_MARKER: &str = "GWMATHZ";

/// Markdown to safe HTML.
///
/// Raw HTML is converted to escaped text, and dangerous link/image schemes
/// are replaced with `#`, so model-authored content cannot inject script.
/// LaTeX math (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) is rendered to MathML
/// server-side. Math inside code spans and fenced code blocks stays literal,
/// and TeX that fails to convert falls back to its escaped source.
pub fn markdown_to_html(text: &str) -> String {
    // Extract math before parsing: TeX `_`, `*`, and `\` must not be
    // interpreted as Markdown emphasis or escapes.
    let segments = extract_math(text);
    let mut source = String::new();
    if segments.is_empty() {
        source.push_str(text);
    } else {
        let mut last = 0;
        for (index, segment) in segments.iter().enumerate() {
            source.push_str(&text[last..segment.start]);
            let _ = write!(source, "{MATH_MARKER}{index}{MATH_MARKER}");
            last = segment.end;
        }
        source.push_str(&text[last..]);
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(&source, options).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_dest(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_dest(dest_url),
            title,
            id,
        }),
        other => other,
    });

    let mut out = String::new();
    html::push_html(&mut out, parser);

    if !segments.is_empty() {
        out = substitute_math(&out, &segments);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MathSegment {
    /// Byte range of the math *including* delimiters in the source text.
    start: usize,
    end: usize,
    /// The TeX body between the delimiters.
    tex: String,
    /// True for display math (`$$…$$`, `\[…\]`).
    display: bool,
}

/// Find LaTeX math in Markdown source, skipping fenced code blocks and
/// inline code spans. A `$` opener must be followed by a non-space,
/// non-digit character, and a `$` closer must hug the content and not be
/// followed by a digit, so prices like `$5 and $10` stay plain text.
fn extract_math(text: &str) -> Vec<MathSegment> {
    let bytes = text.as_bytes();
    let protected = protected_ranges(text);
    let in_protected = |pos: usize| protected.iter().any(|&(s, e)| pos >= s && pos < e);

    let mut segments = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        if in_protected(pos) {
            pos += 1;
            continue;
        }
        if bytes[pos] == b'\\' {
            let (open_len, close, display) = match bytes.get(pos + 1) {
                Some(b'(') => (2usize, "\\)", false),
                Some(b'[') => (2usize, "\\]", true),
                _ => {
                    pos += 1;
                    continue;
                }
            };
            let body_start = pos + open_len;
            if let Some(close_pos) = find_unprotected(text, close, body_start, &protected) {
                let tex = text[body_start..close_pos].trim();
                if !tex.is_empty() {
                    let end = close_pos + close.len();
                    segments.push(MathSegment {
                        start: pos,
                        end,
                        tex: tex.to_string(),
                        display,
                    });
                    pos = end;
                    continue;
                }
            }
            pos += 2;
            continue;
        }
        if bytes[pos] == b'$' {
            let double = bytes.get(pos + 1) == Some(&b'$');
            let delim_len = if double { 2 } else { 1 };
            let body_start = pos + delim_len;
            if !double {
                // Opening `$` must hug the content and not start a price.
                match bytes.get(body_start) {
                    Some(c) if !c.is_ascii_whitespace() && !c.is_ascii_digit() => {}
                    _ => {
                        pos += 1;
                        continue;
                    }
                }
            }
            if let Some(close_pos) = find_unprotected(
                text,
                if double { "$$" } else { "$" },
                body_start,
                &protected,
            ) {
                let end = close_pos + delim_len;
                // Closing `$` must hug the content and not precede a digit
                // (`… $5 and $10 …` must not pair the two prices).
                let closes_inline = !double
                    && close_pos > body_start
                    && !bytes[close_pos - 1].is_ascii_whitespace();
                let followed_by_digit = bytes.get(end).is_some_and(|c| c.is_ascii_digit());
                if (double || closes_inline) && !followed_by_digit {
                    let tex = text[body_start..close_pos].trim();
                    if !tex.is_empty() {
                        segments.push(MathSegment {
                            start: pos,
                            end,
                            tex: tex.to_string(),
                            display: double,
                        });
                        pos = end;
                        continue;
                    }
                }
                // Unusable closer: resume scanning just after it.
                pos = close_pos + 1;
                continue;
            }
            pos += delim_len;
            continue;
        }
        pos += 1;
    }
    segments
}

/// Find the next occurrence of `needle` that is not inside a protected
/// range, starting at `from`.
fn find_unprotected(
    text: &str,
    needle: &str,
    from: usize,
    protected: &[(usize, usize)],
) -> Option<usize> {
    let mut search = from;
    while search <= text.len() {
        let found = text[search..].find(needle)? + search;
        let inside = protected.iter().any(|&(s, e)| found >= s && found < e);
        if !inside {
            return Some(found);
        }
        search = found + 1;
    }
    None
}

/// Byte ranges the math scanner must not touch: fenced code blocks and
/// inline code spans.
fn protected_ranges(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();

    // Fenced code blocks (``` or ~~~).
    let mut fence: Option<(u8, usize)> = None; // (fence char, block start)
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed.as_bytes().first().copied();
        let run = trimmed
            .as_bytes()
            .iter()
            .take_while(|&&c| c == marker.unwrap_or(b'\0'))
            .count();
        match (fence, marker) {
            (None, Some(c @ (b'`' | b'~'))) if run >= 3 => {
                fence = Some((c, offset));
            }
            (Some((c, start)), Some(m)) if m == c && trimmed.trim().len() == run && run >= 3 => {
                ranges.push((start, offset + line.len()));
                fence = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    if let Some((_, start)) = fence {
        ranges.push((start, bytes.len()));
    }

    // Inline code spans (backtick runs with a matching closing run).
    let mut pos = 0;
    while pos < bytes.len() {
        if ranges.iter().any(|&(s, e)| pos >= s && pos < e) {
            pos += 1;
            continue;
        }
        if bytes[pos] == b'`' {
            let run_len = bytes[pos..].iter().take_while(|&&c| c == b'`').count();
            let run_end = pos + run_len;
            let mut search = run_end;
            let mut close_end = None;
            while search < bytes.len() {
                if ranges.iter().any(|&(s, e)| search >= s && search < e) {
                    search += 1;
                    continue;
                }
                if bytes[search] == b'`' {
                    let close_len = bytes[search..].iter().take_while(|&&c| c == b'`').count();
                    if close_len == run_len {
                        close_end = Some(search + close_len);
                        break;
                    }
                    search += close_len;
                    continue;
                }
                search += 1;
            }
            if let Some(close_end) = close_end {
                ranges.push((pos, close_end));
                pos = close_end;
                continue;
            }
            pos = run_end;
            continue;
        }
        pos += 1;
    }

    ranges.sort_unstable();
    ranges
}

/// Render one extracted math segment: MathML Core on success, the escaped
/// TeX source on failure.
fn math_html(segment: &MathSegment) -> String {
    let display = if segment.display {
        MathDisplay::Block
    } else {
        MathDisplay::Inline
    };
    let class = if segment.display {
        "math-block"
    } else {
        "math-inline"
    };
    let converter = LatexToMathML::new(MathCoreConfig::default(), Vec::new());
    let converted = converter
        .ok()
        .and_then(|c| c.convert_with_local_state(&segment.tex, display).ok());
    match converted {
        Some(result) => format!("<span class=\"{class}\">{}</span>", result.mathml),
        None => format!(
            "<code class=\"math-source\">{}</code>",
            escape(&segment.tex)
        ),
    }
}

/// Swap each `GWMATHZ<i>GWMATHZ` placeholder for its rendered MathML.
fn substitute_math(html: &str, segments: &[MathSegment]) -> String {
    let mut out = html.to_string();
    for (index, segment) in segments.iter().enumerate() {
        let placeholder = format!("{MATH_MARKER}{index}{MATH_MARKER}");
        out = out.replace(&placeholder, &math_html(segment));
    }
    out
}

fn safe_dest(dest: CowStr<'_>) -> CowStr<'_> {
    let compact: String = dest
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    let lower = compact.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:")
    {
        CowStr::Borrowed("#")
    } else {
        dest
    }
}

/// `812` → `812`, `34_567` → `35k`.
fn format_k(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{}k", (tokens + 500) / 1000)
    } else {
        tokens.to_string()
    }
}

/// The status line: a telemetry strip with phase, model, key hints,
/// and context usage on the right. With more than one switchable model,
/// the model entry becomes a picker.
pub fn status_html(tab: &Tab, models: &[ModelSummary]) -> String {
    // Mode controls are rendered with status updates, including busy changes.
    let mut out = String::from("<div class=\"status-inner\">");
    for mode in ["build", "plan"] {
        let _ = write!(
            out,
            "<button type=\"button\" class=\"mode-button\" data-mode=\"{mode}\" aria-pressed=\"{}\" {}>{mode}</button>",
            tab.agent == mode,
            if tab.busy() { "disabled" } else { "" }
        );
    }
    if models.len() > 1 {
        // The tab's current selection: its known selector, or the catalog
        // entry matching the server-reported default label.
        let selected = tab.model_selector.clone().or_else(|| {
            models
                .iter()
                .find(|model| Some(&model.label) == tab.model_label.as_ref())
                .map(|model| model.selector.clone())
        });
        let _ = write!(
            out,
            "<label class=\"kv\"><span class=\"k\">model</span><select class=\"model-picker\" data-model-picker data-tab=\"{}\" aria-label=\"switch model\"{}>",
            tab.id,
            if tab.busy() { " disabled" } else { "" }
        );
        for model in models {
            let _ = write!(
                out,
                "<option value=\"{}\"{}>{}</option>",
                escape(&model.selector),
                if selected.as_deref() == Some(model.selector.as_str()) {
                    " selected"
                } else {
                    ""
                },
                escape(&model.label)
            );
        }
        out.push_str("</select></label>");
    } else if let Some(label) = &tab.model_label {
        let _ = write!(
            out,
            "<span class=\"kv\"><span class=\"k\">model</span><span class=\"v model\">{}</span></span>",
            escape(label)
        );
    }
    match (&tab.model.phase, tab.model.status.as_deref()) {
        (Phase::Idle, _) => out.push_str("<span class=\"phase\"><i class=\"dot\"></i>idle</span>"),
        // A transient status (e.g. LLM retry backoff) replaces the plain
        // "thinking…" so a rate-limit wait is visible in the browser.
        (Phase::Busy, Some(status)) => {
            let label = if status == "Compacting context…" {
                "COMPACTING...".to_string()
            } else {
                format!("thinking… {}", escape(status))
            };
            let _ = write!(
                out,
                "<span class=\"phase busy\"><i class=\"dot\"></i>{label}</span>"
            );
        }
        (Phase::Busy, None) => {
            out.push_str("<span class=\"phase busy\"><i class=\"dot\"></i>thinking…</span>");
        }
    }
    // The idle key hints live in the composer placeholder; only the busy
    // hint needs its own spot in the status strip.
    if tab.model.phase == Phase::Busy {
        let _ = write!(
            out,
            "<span class=\"hint\">Enter steer · Esc interrupt</span>"
        );
    }

    if let Some((used, limit)) = tab.context.filter(|(used, limit)| *used > 0 && *limit > 0) {
        let pct = (used * 100 / limit).min(100);
        let class = if pct >= 80 { "usage warn" } else { "usage" };
        let _ = write!(
            out,
            "<span class=\"{class}\" title=\"{used} / {limit} tokens\">\
             <span class=\"v\">{} / {} ({}%)</span></span>",
            format_k(used),
            format_k(limit),
            pct
        );
    }
    // Session cache hit rate: provider-reported prompt-cache usage, summed
    // over every LLM request. Hidden until the provider reports some.
    if let Some(usage) = tab.cache.filter(|u| u.prompt_tokens() > 0) {
        let rate = usage.cache_hit_rate_percent().unwrap_or(0);
        let _ = write!(
            out,
            "<span class=\"kv cache\" title=\"provider-reported prompt-cache hits, {} tokens cached of {} prompt\">\
             <span class=\"k\">cache</span><span class=\"v\">{rate}%</span></span>",
            usage.cached_tokens,
            usage.prompt_tokens()
        );
    }
    out.push_str("</div>");
    out
}

pub fn sidebar_tools_content_html(tools: &Result<Vec<String>, String>) -> String {
    let mut out = String::from("<section class=\"side-section\">");
    match tools {
        Ok(tools) if tools.is_empty() => out.push_str("<p class=\"side-note\">no tools</p>"),
        Ok(tools) => {
            let _ = write!(
                out,
                "<p class=\"side-note tool-count\">{} available</p><ul class=\"side-list\">",
                tools.len()
            );
            for name in tools {
                let _ = write!(out, "<li>{}</li>", escape(name));
            }
            out.push_str("</ul>");
        }
        Err(e) => {
            let _ = write!(
                out,
                "<p class=\"side-note error\">tools unavailable: {}</p>",
                escape(e)
            );
        }
    }
    out.push_str("</section>");
    out
}

/// Render the sidebar's tabbed contents. FILES is intentionally first and
/// active by default; the browser only changes the active panel locally.
#[allow(dead_code)]
pub fn sidebar_tabs_html(
    tools: &Result<Vec<String>, String>,
    working_directory: Option<&str>,
    root: Option<&str>,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) -> String {
    sidebar_tabs_with_workspaces_html(
        tools,
        root.or(working_directory),
        &[],
        "cwd",
        expanded,
        cache,
    )
}

pub fn sidebar_tabs_with_workspaces_html(
    tools: &Result<Vec<String>, String>,
    working_directory: Option<&str>,
    workspaces: &[WorkspaceSummary],
    active_workspace: &str,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) -> String {
    let tools_html = sidebar_tools_content_html(tools);
    let files_html = file_tree_with_workspaces_html(
        working_directory,
        workspaces,
        active_workspace,
        expanded,
        cache,
    );

    format!(
        "<div class=\"sidebar-tabs\" role=\"tablist\" aria-label=\"Sidebar views\"><button type=\"button\" class=\"sidebar-tab active\" role=\"tab\" aria-selected=\"true\" aria-controls=\"sidebar-files\" data-sidebar-tab=\"files\">FILES</button><button type=\"button\" class=\"sidebar-tab\" role=\"tab\" aria-selected=\"false\" aria-controls=\"sidebar-tools\" data-sidebar-tab=\"tools\">TOOLS</button></div><div id=\"sidebar-files\" class=\"sidebar-panel active\" role=\"tabpanel\">{files_html}</div><div id=\"sidebar-tools\" class=\"sidebar-panel\" role=\"tabpanel\" hidden>{tools_html}</div>"
    )
}

/// Render the left session manager: workspace groups with their sessions.
pub fn session_sidebar_html(
    tabs: &[TabInfo],
    workspaces: &[WorkspaceSummary],
    active: u64,
) -> String {
    let mut workspace_ids: Vec<String> = workspaces.iter().map(|workspace| workspace.id.clone()).collect();
    for tab in tabs {
        if !workspace_ids.iter().any(|id| id == &tab.workspace) {
            workspace_ids.push(tab.workspace.clone());
        }
    }

    let mut out = String::from(
        "<aside class=\"session-sidebar\" aria-label=\"Sessions by workspace\"><header class=\"session-sidebar-header\"><strong>SESSIONS</strong></header><div class=\"session-workspace-groups\">",
    );
    for workspace_id in workspace_ids {
        let summary = workspaces.iter().find(|workspace| workspace.id == workspace_id);
        let path = summary
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| workspace_id.strip_prefix("runtime:").unwrap_or(&workspace_id).to_string());
        let label = summary
            .map(workspace_label)
            .unwrap_or_else(|| workspace_id.clone());
        let _ = write!(
            out,
            "<section class=\"session-workspace-group\"><header class=\"session-workspace-header\"><div class=\"session-workspace-heading\"><strong>{}</strong><code title=\"{}\">{}</code></div><button type=\"button\" class=\"session-new\" data-session-new-workspace=\"{}\" data-session-new-path=\"{}\" title=\"New session in {}\" aria-label=\"New session in {}\">＋</button></header><div class=\"session-list\">",
            escape(&label),
            escape(&path),
            escape(&path),
            escape(&workspace_id),
            escape(&path),
            escape(&label),
            escape(&label),
        );
        let group_tabs: Vec<&TabInfo> = tabs
            .iter()
            .filter(|tab| tab.workspace == workspace_id)
            .collect();
        if group_tabs.is_empty() {
            out.push_str("<p class=\"session-empty\">No sessions</p>");
        } else {
            for tab in group_tabs {
                let active_class = if tab.id == active { " active" } else { "" };
                let busy = if tab.busy { " busy" } else { "" };
                let _ = write!(
                    out,
                    "<div class=\"session-row{active_class}{busy}\"><button type=\"button\" class=\"session-select\" data-session-tab=\"{}\" title=\"{}\"><span class=\"session-status\"></span><span class=\"session-label\">{}</span>{}</button><button type=\"button\" class=\"session-close\" data-session-close=\"{}\" title=\"Close session\" aria-label=\"Close {}\">×</button></div>",
                    tab.id,
                    escape(&tab.label),
                    escape(&tab.label),
                    if tab.busy { "<span class=\"session-spinner\" aria-label=\"working\"></span>" } else { "" },
                    tab.id,
                    escape(&tab.label),
                );
            }
        }
        out.push_str("</div></section>");
    }
    out.push_str("</div></aside>");
    out
}

#[allow(dead_code)]
pub fn file_tree_html(
    root: Option<&str>,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) -> String {
    file_tree_with_workspaces_html(root, &[], "cwd", expanded, cache)
}

pub fn file_tree_with_workspaces_html(
    root: Option<&str>,
    workspaces: &[WorkspaceSummary],
    active_workspace: &str,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) -> String {
    let root_label = root.unwrap_or("files");
    let mut out =
        String::from("<section class=\"side-section file-tree-section\" id=\"file-tree\">");
    let _ = write!(
        out,
        "<div class=\"tree-toolbar\"><p class=\"tree-root\" title=\"{}\">{}</p>{}<span class=\"tree-actions\"><button class=\"tree-upload\" type=\"button\" data-tree-upload title=\"Upload files\" aria-label=\"Upload files\">⬆</button><button class=\"tree-refresh\" type=\"button\" data-tree-refresh title=\"Refresh files\" aria-label=\"Refresh files\">↻</button></span></div><ul class=\"tree-list\">",
        escape(root_label),
        escape(root_label),
        workspace_selector_html(root_label, workspaces, active_workspace)
    );
    if let Some(entries) = cache.get("") {
        for entry in entries {
            render_tree_entry(&mut out, entry, &entry.name, 0, expanded, cache);
        }
    } else {
        out.push_str("<li class=\"tree-empty\">loading files…</li>");
    }
    out.push_str("</ul></section>");
    out
}

fn workspace_selector_html(
    root: &str,
    workspaces: &[WorkspaceSummary],
    active: &str,
) -> String {
    let mut out = String::from("<span class=\"workspace-controls\">");
    if workspaces.len() > 1 {
        out.push_str("<label class=\"workspace-picker\"><span>ROOT</span><select data-workspace-select>");
        for workspace in workspaces {
            let selected = if workspace.id == active { " selected" } else { "" };
            let label = workspace_label(workspace);
            let _ = write!(
                out,
                "<option value=\"{}\"{}>{}</option>",
                escape(&workspace.id),
                selected,
                escape(&label)
            );
        }
        out.push_str("</select></label>");
    }
    let _ = write!(
        out,
        "<button type=\"button\" class=\"workspace-add\" data-workspace-add data-workspace-browse-path=\"{}\" title=\"Add a server directory\" aria-label=\"Add a server directory\">＋</button>",
        escape(root)
    );
    out.push_str("</span>");
    out
}

fn workspace_label(workspace: &WorkspaceSummary) -> String {
    if workspace.id.starts_with("runtime:") {
        std::path::Path::new(&workspace.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&workspace.id)
            .to_string()
    } else {
        workspace.id.clone()
    }
}

pub fn workspace_browser_html(
    path: &str,
    parent: Option<&str>,
    entries: &[DirectoryEntry],
) -> String {
    let mut out = String::from(
        "<section id=\"workspace-browser\" class=\"workspace-browser\" role=\"dialog\" aria-label=\"Choose workspace directory\"><div class=\"workspace-browser-card\"><header><strong>Choose server directory</strong><button type=\"button\" data-workspace-browser-close aria-label=\"Close\">×</button></header>",
    );
    let _ = write!(
        out,
        "<p class=\"workspace-browser-path\" title=\"{}\">{}</p><div class=\"workspace-browser-actions\">",
        escape(path),
        escape(path)
    );
    if let Some(parent) = parent {
        let _ = write!(
            out,
            "<button type=\"button\" data-workspace-browse-path=\"{}\">← parent</button>",
            escape(parent)
        );
    }
    let _ = write!(
        out,
        "<button type=\"button\" data-workspace-add-path=\"{}\">Use this directory</button></div><ul class=\"workspace-browser-list\">",
        escape(path)
    );
    for entry in entries {
        let _ = write!(
            out,
            "<li><button type=\"button\" data-workspace-browse-path=\"{}\">📁 {}</button></li>",
            escape(&entry.path),
            escape(&entry.name)
        );
    }
    if entries.is_empty() {
        out.push_str("<li class=\"workspace-browser-empty\">No child directories</li>");
    }
    out.push_str("</ul></div></section>");
    out
}

/// Render the workdir choice shown before a new session tab is created.
pub fn session_workspace_picker_html(
    workspaces: &[WorkspaceSummary],
    browse_path: &str,
) -> String {
    let mut out = String::from(
        "<section id=\"session-workspace-picker\" class=\"workspace-browser\" role=\"dialog\" aria-label=\"Choose workspace for new session\"><div class=\"workspace-browser-card session-picker-card\"><header><strong>New session · choose workdir</strong><button type=\"button\" data-new-session-close aria-label=\"Close\">×</button></header><p class=\"workspace-picker-note\">The session will start in the selected server directory.</p><ul class=\"workspace-browser-list session-workspace-list\">",
    );
    for workspace in workspaces {
        let _ = write!(
            out,
            "<li><button type=\"button\" class=\"session-workspace-option\" data-new-session-create data-new-session-workspace=\"{}\" data-new-session-path=\"{}\"><span class=\"session-workspace-name\">{}</span><code>{}</code></button></li>",
            escape(&workspace.id),
            escape(&workspace.path),
            escape(&workspace_label(workspace)),
            escape(&workspace.path),
        );
    }
    let _ = write!(
        out,
        "<li><button type=\"button\" class=\"session-workspace-browse\" data-new-session-browse-path=\"{}\">＋ Browse server directories…</button></li></ul></div></section>",
        escape(browse_path)
    );
    out
}

/// Render the directory browser used by the new-session picker. Its choose
/// buttons create a tab without creating an upstream session yet.
pub fn new_session_browser_html(
    path: &str,
    parent: Option<&str>,
    entries: &[DirectoryEntry],
) -> String {
    let mut out = String::from(
        "<section id=\"session-workspace-picker\" class=\"workspace-browser\" role=\"dialog\" aria-label=\"Choose workspace for new session\"><div class=\"workspace-browser-card session-picker-card\"><header><strong>New session · choose workdir</strong><button type=\"button\" data-new-session-close aria-label=\"Close\">×</button></header>",
    );
    let _ = write!(
        out,
        "<p class=\"workspace-browser-path\" title=\"{}\">{}</p><div class=\"workspace-browser-actions\">",
        escape(path),
        escape(path)
    );
    if let Some(parent) = parent {
        let _ = write!(
            out,
            "<button type=\"button\" data-new-session-browse-path=\"{}\">← parent</button>",
            escape(parent)
        );
    }
    let workspace_id = format!("runtime:{path}");
    let _ = write!(
        out,
        "<button type=\"button\" data-new-session-create data-new-session-workspace=\"{}\" data-new-session-path=\"{}\">Use this directory</button></div><ul class=\"workspace-browser-list\">",
        escape(&workspace_id),
        escape(path)
    );
    for entry in entries {
        let _ = write!(
            out,
            "<li><button type=\"button\" data-new-session-browse-path=\"{}\">📁 {}</button></li>",
            escape(&entry.path),
            escape(&entry.name)
        );
    }
    if entries.is_empty() {
        out.push_str("<li class=\"workspace-browser-empty\">No child directories</li>");
    }
    out.push_str("</ul></div></section>");
    out
}

fn render_tree_entry(
    out: &mut String,
    entry: &FileEntry,
    path: &str,
    depth: usize,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) {
    if entry.is_dir {
        let open = expanded.contains(path);
        let _ = write!(
            out,
            "<li class=\"tree-entry\"><button type=\"button\" class=\"tree-dir\" data-path=\"{}\" style=\"--depth:{}\" aria-expanded=\"{}\"><span class=\"tree-caret\">{}</span><span class=\"tree-name\">{}</span></button>",
            escape(path),
            depth,
            open,
            if open { "▾" } else { "▸" },
            escape(&entry.name)
        );
        if open {
            out.push_str("<ul class=\"tree-children\">");
            if let Some(children) = cache.get(path) {
                for child in children {
                    let child_path = format!("{path}/{}", child.name);
                    render_tree_entry(out, child, &child_path, depth + 1, expanded, cache);
                }
            } else {
                out.push_str("<li class=\"tree-empty\">loading…</li>");
            }
            out.push_str("</ul>");
        }
        out.push_str("</li>");
    } else {
        let _ = write!(
            out,
            "<li class=\"tree-entry\"><button type=\"button\" class=\"tree-file\" data-path=\"{}\" style=\"--depth:{}\"><span class=\"tree-caret\"></span><span class=\"tree-name\">{}</span></button></li>",
            escape(path),
            depth,
            escape(&entry.name)
        );
    }
}

pub fn file_preview_html(path: &str, content: &str, truncated: bool) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<header class=\"preview-header\"><div class=\"preview-title\" title=\"{}\">{}</div><button type=\"button\" class=\"icon-button\" data-preview-close title=\"Close preview\" aria-label=\"Close preview\">×</button></header><pre class=\"preview-content\"><code>{}</code></pre>",
        escape(path),
        escape(path),
        escape(content)
    );
    if truncated {
        out.push_str("<p class=\"preview-truncated\">Preview limited to the first 512 KB.</p>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TabInfo;
    use crate::workspace::Workspace;
    use gewu_protocol::AgentEvent;
    use gewu_protocol::TokenUsage;

    fn tab_with_blocks(blocks: Vec<Block>) -> (Workspace, u64) {
        let mut ws = Workspace::new();
        let id = ws.open_blank();
        let tab = ws.tab_mut(id).unwrap();
        tab.model.blocks = blocks;
        (ws, id)
    }

    #[test]
    fn escapes_html_sensitive_text() {
        assert_eq!(
            escape(r#"<script>&"x" '</script>"#),
            "&lt;script&gt;&amp;&quot;x&quot; &#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn inline_math_renders_as_mathml() {
        let html = markdown_to_html("Energy is $E = mc^2$ indeed.");
        assert!(html.contains("<math"), "{html}");
        assert!(html.contains("math-inline"), "{html}");
        assert!(!html.contains('$'), "{html}");
    }

    #[test]
    fn display_math_renders_as_block() {
        let html = markdown_to_html("Consider:\n\n$$\\int_0^\\infty e^{-x}\\,dx = 1$$\n\ndone");
        assert!(html.contains("<math"), "{html}");
        assert!(html.contains("math-block"), "{html}");
    }

    #[test]
    fn backslash_math_delimiters_work() {
        let html = markdown_to_html("Inline \\(a^2 + b^2\\) and display:\n\\[c^2\\]");
        assert_eq!(html.matches("<math").count(), 2, "{html}");
    }

    #[test]
    fn multiline_display_math_works() {
        let html = markdown_to_html("$$\na + b\n$$");
        assert!(html.contains("math-block"), "{html}");
        assert!(html.contains("<math"), "{html}");
    }

    #[test]
    fn math_in_code_spans_and_fences_stays_literal() {
        let html = markdown_to_html("Fenced:\n\n```\n$x^2$\n```\n\nInline `$x^2$` code.");
        assert!(!html.contains("<math"), "{html}");
        assert!(html.contains("$x^2$"), "{html}");
    }

    #[test]
    fn prices_stay_plain_text() {
        let html = markdown_to_html("It costs $5 and $10 total.");
        assert!(!html.contains("<math"), "{html}");
        assert!(html.contains("$5 and $10"), "{html}");
    }

    #[test]
    fn unmatched_dollar_stays_literal() {
        for text in ["100$ profit", "a $ x", "trailing $"] {
            let html = markdown_to_html(text);
            assert!(!html.contains("<math"), "{text}: {html}");
        }
    }

    #[test]
    fn tex_specials_are_not_markdown_and_are_escaped() {
        let html = markdown_to_html("$a_i * b$ and $x < 1$");
        assert!(!html.contains("<em>"), "{html}");
        assert_eq!(html.matches("<math").count(), 2, "{html}");
        // A raw `<` before a space would mean unescaped TeX text.
        assert!(!html.contains("< 1"), "{html}");
    }

    #[test]
    fn malformed_tex_falls_back_to_escaped_source() {
        let html = markdown_to_html("$\\notacommand{x}$");
        assert!(html.contains("math-source"), "{html}");
        assert!(
            html.contains("&lt;") || html.contains("notacommand"),
            "{html}"
        );
    }

    #[test]
    fn mathcal_and_dots_convert() {
        // Regression: real assistant output the predecessor crate could not
        // convert (unsupported `\mathcal` / `\dots`).
        let html = markdown_to_html(
            "a **guidance graph** $\\mathcal{G} = (\\mathcal{V}, \\mathcal{E})$: and $X_1, \\dots, X_K$.",
        );
        assert!(!html.contains("math-source"), "{html}");
        assert_eq!(html.matches("<math").count(), 2, "{html}");
    }

    #[test]
    fn tex_error_messages_cannot_inject_html() {
        // TeX parse errors echo parts of the input; the fallback must
        // escape them rather than trust the crate's inline error text.
        let html = markdown_to_html("$\\notacommand{<b>}$");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("math-source"), "{html}");
    }

    #[test]
    fn sidebar_lists_escaped_tool_names() {
        let html = sidebar_tools_content_html(&Ok(vec!["read".into(), "<bad>".into()]));
        assert!(html.contains("2 available"), "{html}");
        assert!(html.contains("<li>read</li>"), "{html}");
        assert!(html.contains("<li>&lt;bad&gt;</li>"), "{html}");
        assert!(!html.contains("side-cwd"), "{html}");
    }

    #[test]
    fn sidebar_reports_unavailable_tools() {
        let html = sidebar_tools_content_html(&Err("boom <x>".into()));
        assert!(html.contains("tools unavailable: boom &lt;x&gt;"), "{html}");
        assert!(!html.contains("<ul"), "{html}");
    }

    #[test]
    fn sidebar_escapes_working_directory() {
        let html = file_tree_html(Some("/tmp/<bad>&dir"), &BTreeSet::new(), &BTreeMap::new());
        assert!(html.contains("/tmp/&lt;bad&gt;&amp;dir"), "{html}");
        assert!(!html.contains("<bad>"), "{html}");
    }

    #[test]
    fn sidebar_files_tab_is_active_by_default() {
        let mut cache = BTreeMap::new();
        cache.insert(
            String::new(),
            vec![FileEntry {
                name: "main.rs".into(),
                is_dir: false,
            }],
        );
        let html = sidebar_tabs_html(
            &Ok(vec!["read".into()]),
            Some("/tmp/project"),
            Some("/tmp/project"),
            &BTreeSet::new(),
            &cache,
        );
        assert!(
            html.contains("data-sidebar-tab=\"files\">FILES</button>"),
            "{html}"
        );
        assert!(
            html.contains("data-sidebar-tab=\"tools\">TOOLS</button>"),
            "{html}"
        );
        assert!(
            html.contains("id=\"sidebar-files\" class=\"sidebar-panel active\""),
            "{html}"
        );
        assert!(
            html.contains("id=\"sidebar-tools\" class=\"sidebar-panel\" role=\"tabpanel\" hidden"),
            "{html}"
        );
        assert!(!html.contains("<h2 class=\"side-title\">files"), "{html}");
        assert!(!html.contains("<h2 class=\"side-title\">tools"), "{html}");
        assert!(html.contains("data-tree-refresh"), "{html}");
    }

    #[test]
    fn file_tree_renders_configured_workspace_selector() {
        let html = file_tree_with_workspaces_html(
            Some("/tmp/frontend"),
            &[
                WorkspaceSummary {
                    id: "cwd".into(),
                    path: "/tmp/project".into(),
                },
                WorkspaceSummary {
                    id: "frontend".into(),
                    path: "/tmp/frontend".into(),
                },
            ],
            "frontend",
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert!(html.contains("data-workspace-select"), "{html}");
        assert!(html.contains("value=\"frontend\" selected"), "{html}");
    }

    #[test]
    fn workspace_toolbar_always_has_add_directory_action() {
        let html = file_tree_with_workspaces_html(
            Some("/tmp/project"),
            &[],
            "cwd",
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert!(html.contains("data-workspace-add"), "{html}");
    }

    #[test]
    fn workspace_browser_renders_server_directory_choices() {
        let html = workspace_browser_html(
            "/tmp",
            Some("/"),
            &[DirectoryEntry {
                name: "project".into(),
                path: "/tmp/project".into(),
            }],
        );
        assert!(html.contains("data-workspace-browse-path=\"/tmp/project\""), "{html}");
        assert!(html.contains("data-workspace-add-path=\"/tmp\""), "{html}");
    }

    #[test]
    fn new_session_picker_lists_workspaces_and_browse_action() {
        let html = session_workspace_picker_html(
            &[
                WorkspaceSummary {
                    id: "cwd".into(),
                    path: "/data3/litian/projects/GeWu".into(),
                },
                WorkspaceSummary {
                    id: "tachyon".into(),
                    path: "/data3/litian/projects/tachyon_project".into(),
                },
            ],
            "/data3/litian/projects/GeWu",
        );
        assert!(html.contains("id=\"session-workspace-picker\""), "{html}");
        assert!(html.contains("data-new-session-workspace=\"tachyon\""), "{html}");
        assert!(html.contains("data-new-session-browse-path"), "{html}");
    }

    #[test]
    fn new_session_browser_selects_a_runtime_workspace() {
        let html = new_session_browser_html(
            "/data3/litian/projects",
            Some("/data3/litian"),
            &[DirectoryEntry {
                name: "tachyon_project".into(),
                path: "/data3/litian/projects/tachyon_project".into(),
            }],
        );
        assert!(
            html.contains("data-new-session-workspace=\"runtime:/data3/litian/projects\""),
            "{html}"
        );
        assert!(html.contains("data-new-session-create"), "{html}");
        assert!(
            html.contains("data-new-session-browse-path=\"/data3/litian/projects/tachyon_project\""),
            "{html}"
        );
    }

    #[test]
    fn session_sidebar_groups_sessions_by_workspace() {
        let html = session_sidebar_html(
            &[
                TabInfo {
                    id: 1,
                    label: "tachyon work".into(),
                    busy: false,
                    session_id: Some("session-1".into()),
                    workspace: "tachyon".into(),
                },
                TabInfo {
                    id: 2,
                    label: "wiki notes".into(),
                    busy: true,
                    session_id: Some("session-2".into()),
                    workspace: "wiki".into(),
                },
            ],
            &[
                WorkspaceSummary {
                    id: "tachyon".into(),
                    path: "/data3/litian/projects/tachyon_project".into(),
                },
                WorkspaceSummary {
                    id: "wiki".into(),
                    path: "/data/litian/my-wiki".into(),
                },
            ],
            2,
        );
        assert!(html.contains("class=\"session-sidebar\""), "{html}");
        assert!(html.contains("data-session-tab=\"1\""), "{html}");
        assert!(html.contains("data-session-tab=\"2\""), "{html}");
        assert!(html.contains("data-session-new-workspace=\"tachyon\""), "{html}");
        assert!(html.contains("session-row active"), "{html}");
        assert!(html.contains("data-session-close=\"2\""), "{html}");
    }

    #[test]
    fn compaction_renders_as_a_divider_between_original_messages() {
        let (ws, id) = tab_with_blocks(vec![
            Block::User("original question".into()),
            Block::Compaction {
                summary: "## Objective\n- **Continue** safely <script>alert(1)</script>".into(),
            },
            Block::Assistant("continued answer".into()),
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("<details><summary>Context compacted</summary>"));
        assert!(html.contains("<strong>Continue</strong>"));
        assert!(!html.contains("<script>"));
        assert_eq!(
            html.matches("<summary>Context compacted</summary>").count(),
            1
        );
        assert!(html.find("original question").unwrap() < html.find("Context compacted").unwrap());
        assert!(html.find("Context compacted").unwrap() < html.find("continued answer").unwrap());
    }

    #[test]
    fn compacting_status_has_no_thinking_prefix() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        let tab = ws.tab_mut(id).unwrap();
        tab.model.phase = Phase::Busy;
        tab.model.status = Some("Compacting context…".into());
        let html = status_html(tab, &[]);
        assert!(html.contains("COMPACTING..."), "{html}");
        assert!(!html.contains("thinking…"), "{html}");
    }

    #[test]
    fn renders_user_and_assistant_blocks() {
        let (ws, id) = tab_with_blocks(vec![
            Block::User("<b>hi</b>".into()),
            Block::Assistant("**bold**".into()),
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"block user\""), "{html}");
        assert!(!html.contains("class=\"label\""), "{html}");
        assert!(!html.contains(">gewu<"), "{html}");
        assert!(!html.contains(">you<"), "{html}");
        assert!(html.contains("&lt;b&gt;hi&lt;/b&gt;"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(!html.contains("<b>hi</b>"), "user HTML must be escaped");
    }

    #[test]
    fn streaming_assistant_is_markdown_and_escaped() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Assistant("<b>partial".into())]);
        ws.tab_mut(id).unwrap().model.phase = Phase::Busy;
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"body markdown streaming\""), "{html}");
        assert!(!html.contains("class=\"label\""), "{html}");
        assert!(!html.contains(">gewu<"), "{html}");
        assert!(html.contains("&lt;b&gt;partial"), "{html}");
        assert!(!html.contains("<b>partial"), "raw HTML must stay escaped");
    }

    #[test]
    fn markdown_raw_html_is_neutralized() {
        let html = markdown_to_html("<script>alert(1)</script>\n\n**ok**");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("<strong>ok</strong>"), "{html}");
    }

    #[test]
    fn markdown_dangerous_links_are_replaced() {
        let html = markdown_to_html("[x](javascript:alert(1))");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(html.contains("href=\"#\""), "{html}");

        let html = markdown_to_html("[ok](https://example.org)");
        assert!(html.contains("href=\"https://example.org\""), "{html}");
    }

    #[test]
    fn tool_rows_use_readable_names() {
        assert_eq!(tool_row("bash", "ls -la"), ("bash", "ls -la".to_string()));
        assert_eq!(
            tool_row("read_file", "src/a.rs"),
            ("read", "src/a.rs".to_string())
        );
        assert_eq!(tool_row("apply_patch", "p.diff").0, "patch");
        assert_eq!(tool_row("custom_tool", "{}").0, "custom_tool");
    }

    #[test]
    fn single_tool_renders_row_and_output() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a.rs\nb.rs".into()),
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(
            html.contains("class=\"tool-card\" data-card=\"0\""),
            "{html}"
        );
        assert!(html.contains("tool-name\">bash"), "{html}");
        assert!(html.contains("tool-title\">ls"), "{html}");
        assert!(html.contains("a.rs\nb.rs"), "{html}");
        assert!(
            html.contains("class=\"tool depth-0\" data-action=\"toggle_tool\""),
            "the whole card body should be clickable: {html}"
        );
        assert!(html.contains("data-action=\"toggle_tool\""), "{html}");
    }

    #[test]
    fn tool_card_html_renders_one_run() {
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "bash".into(),
                input: "first".into(),
                output: Some("one".into()),
            },
            Block::Assistant("between".into()),
            Block::Tool {
                tool: "bash".into(),
                input: "second".into(),
                output: Some(
                    (1..=10)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            },
        ]);
        ws.tab_mut(id).unwrap().expanded_outputs.insert(2);
        let html = tool_card_html(ws.tab(id).unwrap(), 2).expect("tool card");
        assert!(
            html.contains("class=\"tool-card\" data-card=\"2\""),
            "{html}"
        );
        assert!(html.contains("line 10"), "expanded lines: {html}");
        assert!(!html.contains("first"), "only the requested run: {html}");
        assert!(!html.contains("between"), "{html}");
    }

    #[test]
    fn workflow_cards_draw_the_run_graph() {
        let spec = r#"{"name":"docs","nodes":[
            {"id":"read","agent":"reader","prompt":"p"},
            {"id":"polish","agent":"writer","prompt":"p"}],
            "conditional_edges":{"polish":{"path":"{{nodes.polish.status}}",
            "map":{"ok":"END","failed":"polish"}}}}"#;
        let start = "Started workflow run r-77. It executes in the background; check on it with \
             workflow_status or workflow_wait (run_id: \"r-77\").";
        let snapshot = "run r-77 (docs): running, 1 task(s) done, 0 failed, 1 attempted\n\
             current node: polish\n\
             node read: ok, 1 done, 0 failed\n\
             node polish: pending, 0 done, 0 failed";
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "workflow_start".into(),
                input: spec.into(),
                output: Some(start.into()),
            },
            Block::Tool {
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
            },
        ]);
        let tab = ws.tab(id).unwrap();

        // Start and status share one group card; only the status row gets a
        // graph — the start row stays plain (title + output).
        let html = tool_card_html(tab, 0).expect("start card");
        assert_eq!(html.matches("wf-card").count(), 1, "{html}");
        assert!(html.contains("docs · 2 nodes"), "{html}");

        // The status card finds the spec via the matching run_id and draws
        // the graph with the snapshot's node statuses.
        let html = tool_card_html(tab, 1).expect("status card");
        assert!(html.contains("wf-card"), "{html}");
        assert!(html.contains("wf-ok"), "{html}");
        assert!(html.contains("wf-run-running"), "{html}");
        assert!(html.contains("current: polish"), "{html}");
        // its row title is just the run id
        assert!(html.contains(">r-77</span>"), "{html}");
    }

    #[test]
    fn workflow_groups_collapse_on_user_toggle() {
        let snapshot = "run r-77 (docs): completed, 1 task(s) done, 0 failed, 1 attempted\n\
             node read: ok, 1 done, 0 failed";
        let start = "Started workflow run r-77. It executes in the background.";
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "workflow_start".into(),
                input: r#"{"name":"docs","nodes":[{"id":"read","agent":"reader","prompt":"p"}]}"#
                    .into(),
                output: Some(start.into()),
            },
            Block::Tool {
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
            },
            Block::Tool {
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
            },
        ]);
        // Defaults to expanded: the graphs are visible.
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("group card");
        assert!(html.contains("wf-card"), "{html}");
        assert!(html.contains("−"), "{html}");

        // An explicit collapse (the ToggleGroup path) closes it.
        ws.tab_mut(id).unwrap().collapsed_groups.insert(0);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("group card");
        assert!(!html.contains("wf-card"), "{html}");
        assert!(html.contains(">+ Explored"), "{html}");

        // …and the classifier agrees with what the view rendered.
        let tab = ws.tab(id).unwrap();
        assert!(is_workflow_run(&tab.model.blocks[0..2]));
        assert!(!is_workflow_run(&[Block::Tool {
            tool: "bash".into(),
            input: String::new(),
            output: None,
        }]));
    }

    #[test]
    fn tool_card_html_needs_a_tool_block() {
        let (ws, id) = tab_with_blocks(vec![Block::Assistant("hi".into())]);
        assert!(tool_card_html(ws.tab(id).unwrap(), 0).is_none());
        assert!(tool_card_html(ws.tab(id).unwrap(), 9).is_none());
    }

    #[test]
    fn block_html_renders_one_block() {
        let (mut ws, id) = tab_with_blocks(vec![
            Block::User("hi".into()),
            Block::Assistant("streaming".into()),
        ]);
        let tab = {
            let tab = ws.tab_mut(id).unwrap();
            tab.model.phase = Phase::Busy;
            tab
        };
        let html = block_html(tab, 0).expect("user block");
        assert!(
            html.contains("class=\"block user\" data-block=\"0\""),
            "{html}"
        );
        assert!(!html.contains("streaming"), "{html}");

        let html = block_html(tab, 1).expect("assistant block");
        assert!(
            html.contains("class=\"block assistant\" data-block=\"1\""),
            "{html}"
        );
        assert!(html.contains("body markdown streaming"), "{html}");

        assert!(block_html(tab, 9).is_none());
    }

    #[test]
    fn assistant_body_html_renders_final_markdown() {
        let (ws, id) = tab_with_blocks(vec![
            Block::User("hi".into()),
            Block::Assistant("**done**".into()),
        ]);
        let html = assistant_body_html(ws.tab(id).unwrap(), 1, false).expect("body");
        assert!(html.contains("class=\"body markdown\""), "{html}");
        assert!(html.contains("<strong>done</strong>"), "{html}");
        assert!(!html.contains("streaming"), "{html}");

        // Only assistant blocks have a final body.
        assert!(assistant_body_html(ws.tab(id).unwrap(), 0, false).is_none());
        assert!(assistant_body_html(ws.tab(id).unwrap(), 9, false).is_none());
    }
    #[test]
    fn tool_output_collapses_with_expand_button() {
        let output = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: "seq 10".into(),
            output: Some(output),
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("line 6"), "{html}");
        assert!(!html.contains("line 10"), "{html}");
        assert!(html.contains("… +4 lines"), "{html}");

        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("line 10"), "{html}");
        assert!(!html.contains("+4 lines"), "{html}");
    }

    #[test]
    fn expanded_tool_shows_the_full_command() {
        let long_command = format!("echo {} # padded well past one line", "x".repeat(120));
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: long_command.clone(),
            output: Some("done".into()),
        }]);
        // Collapsed: the command appears only in the ellipsized title row.
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(!html.contains("class=\"tool-input\""), "{html}");

        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool-input\""), "{html}");
        assert!(html.contains(&long_command), "full command visible: {html}");
    }

    #[test]
    fn expanded_output_is_not_capped_at_500_lines() {
        let output = (1..=600)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: "seq 600".into(),
            output: Some(output),
        }]);
        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("line 600"), "{html}");
        assert!(!html.contains("tool-more"), "{html}");
    }

    #[test]
    fn consecutive_tools_group_and_expand() {
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
            },
            Block::Tool {
                tool: "read_file".into(),
                input: "src/main.rs".into(),
                output: Some("b".into()),
            },
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool-card\""), "{html}");
        assert!(html.contains("+ Explored: 1 bash, 1 read"), "{html}");
        assert!(!html.contains("src/main.rs"), "{html}");
        assert!(html.contains("data-action=\"toggle_group\""), "{html}");
        assert!(
            html.contains("class=\"tool-group\" data-block=\"0\" data-action=\"toggle_group\""),
            "the whole group card should be clickable: {html}"
        );

        ws.tab_mut(id).unwrap().expanded_groups.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("− Explored: 1 bash, 1 read"), "{html}");
        assert!(html.contains("src/main.rs"), "{html}");
    }

    #[test]
    fn running_tool_shows_spinner() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: "sleep 30".into(),
            output: None,
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool running depth-0\""), "{html}");
        assert!(
            html.contains("<span class=\"tool-spinner\" aria-hidden=\"true\"></span>"),
            "{html}"
        );
    }

    #[test]
    fn finished_tool_has_no_running_indicator() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a".into()),
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool depth-0\""), "{html}");
        assert!(!html.contains("tool-spinner"), "{html}");
        assert!(!html.contains("running"), "{html}");
    }

    #[test]
    fn running_group_says_exploring() {
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "bash".into(),
                input: "ls".into(),
                output: None,
            },
            Block::Tool {
                tool: "grep".into(),
                input: "TODO".into(),
                output: None,
            },
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("+ Exploring: 1 bash, 1 grep"), "{html}");
        assert!(html.contains("class=\"tool-group running\""), "{html}");
    }

    #[test]
    fn finished_group_has_no_running_class() {
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
            },
            Block::Tool {
                tool: "grep".into(),
                input: "TODO".into(),
                output: Some("b".into()),
            },
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("+ Explored: 1 bash, 1 grep"), "{html}");
        assert!(
            html.contains("class=\"tool-group\" data-block=\"0\""),
            "{html}"
        );
        assert!(!html.contains("tool-spinner"), "{html}");
    }

    #[test]
    fn partial_group_marks_remaining_tool_running() {
        // A mixed run: first tool finished, second still running. The group
        // label pulses and the expanded rows show only the pending spinner.
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
            },
            Block::Tool {
                tool: "grep".into(),
                input: "TODO".into(),
                output: None,
            },
        ]);
        ws.tab_mut(id).unwrap().expanded_groups.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool-group running\""), "{html}");
        assert!(html.contains("Exploring: 1 bash, 1 grep"), "{html}");
        assert!(html.contains("class=\"tool depth-1\""), "{html}");
        assert!(html.contains("tool-spinner"), "{html}");
    }

    #[test]
    fn status_shows_model_and_usage() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        let tab = ws.tab_mut(id).unwrap();
        tab.model_label = Some("gpt-4o".into());
        tab.context = Some((34_000, 128_000));
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("gpt-4o"), "{html}");
        assert!(!html.contains("openai"), "{html}");
        assert!(!html.contains("session"), "{html}");
        assert!(!html.contains("cache"), "{html}");
        assert!(html.contains("34k / 128k (26%)"), "{html}");
        assert!(!html.contains("gauge"), "{html}");
    }

    #[test]
    fn status_renders_model_picker_when_multiple_models() {
        let models = vec![
            ModelSummary {
                selector: "west/big".into(),
                label: "big-model".into(),
            },
            ModelSummary {
                selector: "west/small".into(),
                label: "small-model".into(),
            },
        ];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_selector = Some("west/small".into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("<select class=\"model-picker\" data-model-picker data-tab="),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"west/big\">big-model</option>"),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"west/small\" selected>small-model</option>"),
            "{html}"
        );

        // Without a selector, the default label picks the selected option.
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_label = Some("big-model".into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("<option value=\"west/big\" selected>big-model</option>"),
            "{html}"
        );

        // A single catalog entry stays a plain label.
        let solo = vec![ModelSummary {
            selector: "west/big".into(),
            label: "big-model".into(),
        }];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_label = Some("big-model".into());
        let html = status_html(ws.tab(id).unwrap(), &solo);
        assert!(!html.contains("model-picker"), "{html}");
        assert!(html.contains("class=\"v model\""), "{html}");
    }

    #[test]
    fn status_shows_provider_cache_hit_rate() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        let tab = ws.tab_mut(id).unwrap();
        tab.context = Some((34_000, 128_000));
        tab.cache = Some(TokenUsage {
            input_tokens: 1_000,
            cached_tokens: 9_000,
            ..TokenUsage::default()
        });
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("cache"), "{html}");
        assert!(html.contains(">90%</span>"), "{html}");
        // No provider usage: hidden even when the context usage shows.
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().context = Some((34_000, 128_000));
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(!html.contains("cache"), "{html}");
    }

    #[test]
    fn status_usage_warns_near_limit() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().context = Some((110_000, 128_000));
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("class=\"usage warn\""), "{html}");
        assert!(html.contains("(85%)"), "{html}");
    }

    #[test]
    fn busy_status_shows_interrupt_hint() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model.submit("go".into());
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("thinking…"), "{html}");
        assert!(html.contains("Esc interrupt"), "{html}");
    }

    #[test]
    fn finalized_thought_renders_dim_indicator_with_escaped_text() {
        let (ws, id) = tab_with_blocks(vec![
            Block::User("hi".into()),
            Block::Thought {
                text: "Thought 5.6s".into(),
                reasoning: String::new(),
                started: None,
            },
            Block::Assistant("answer".into()),
        ]);
        let html = block_html(ws.tab(id).unwrap(), 1).expect("thought block renders");
        assert!(html.contains("class=\"block thought\""), "{html}");
        assert!(html.contains("class=\"body plain thought\""), "{html}");
        assert!(html.contains("Thought 5.6s"), "{html}");
        // Without reasoning content there is no toggle.
        assert!(!html.contains("toggle_thought"), "{html}");

        // Block text is escaped like other plain bodies.
        let (ws, id) = tab_with_blocks(vec![Block::Thought {
            text: "Thought <b>1.0s</b>".into(),
            reasoning: String::new(),
            started: None,
        }]);
        let html = block_html(ws.tab(id).unwrap(), 0).unwrap();
        assert!(html.contains("Thought &lt;b&gt;1.0s&lt;/b&gt;"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
    }

    #[test]
    fn thought_with_reasoning_renders_a_toggle() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Thought {
            text: "Thought 5.6s".into(),
            reasoning: "step <one>".into(),
            started: None,
        }]);
        let html = block_html(ws.tab(id).unwrap(), 0).expect("thought block renders");
        // Collapsed by default: a '+' toggle, no reasoning text.
        assert!(html.contains("data-action=\"toggle_thought\""), "{html}");
        assert!(html.contains(">+ Thought 5.6s</button>"), "{html}");
        assert!(!html.contains("thought-body"), "{html}");

        // Expanded: a '−' toggle and the escaped reasoning text.
        ws.tab_mut(id).unwrap().expanded_thoughts.insert(0);
        let html = block_html(ws.tab(id).unwrap(), 0).unwrap();
        assert!(html.contains(">− Thought 5.6s</button>"), "{html}");
        assert!(html.contains("class=\"thought-body\""), "{html}");
        assert!(html.contains("step &lt;one&gt;"), "{html}");
        assert!(!html.contains("<one>"), "{html}");
    }

    #[test]
    fn live_thought_renders_streaming_marker_when_last_and_busy() {
        let (mut ws, id) = tab_with_blocks(vec![Block::User("hi".into())]);
        {
            let tab = ws.tab_mut(id).unwrap();
            tab.model.submit("go".into()); // busy; adds a second user block
            tab.model
                .apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });
        }
        // Blocks: user, user (from submit), thought.
        let html = block_html(ws.tab(id).unwrap(), 2).expect("thought block renders");
        assert!(html.contains("body plain thought streaming"), "{html}");
        assert!(html.contains("Thinking…"), "{html}");

        // Once a visible block follows, the thought is no longer the
        // streaming tail and renders settled.
        {
            let tab = ws.tab_mut(id).unwrap();
            tab.model.apply_event(&AgentEvent::ContentDelta {
                delta: "answer".into(),
            });
        }
        // Blocks: user, user (from submit), thought, assistant.
        let html = block_html(ws.tab(id).unwrap(), 2).unwrap();
        assert!(html.contains("body plain thought\""), "{html}");
        assert!(!html.contains("streaming"), "{html}");
    }

    #[test]
    fn assistant_body_html_serves_thought_bodies_for_in_place_updates() {
        let (mut ws, id) = tab_with_blocks(vec![Block::User("hi".into())]);
        let tab = ws.tab_mut(id).unwrap();
        tab.model.submit("go".into());
        tab.model
            .apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });

        // Blocks: user, user (from submit), thought.
        let html = assistant_body_html(ws.tab(id).unwrap(), 2, true).expect("thought body");
        assert!(html.contains("body plain thought streaming"), "{html}");
        assert!(html.contains("Thinking…"), "{html}");

        // Non-thought, non-assistant blocks still return None.
        assert_eq!(assistant_body_html(ws.tab(id).unwrap(), 0, true), None);
    }
}
