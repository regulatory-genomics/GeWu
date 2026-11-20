//! HTML rendering for the web GUI.
//!
//! Transcript blocks, Markdown, tool runs, and the status line. Everything
//! user- or model-authored is escaped, and raw HTML in Markdown is neutralized.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use gewu_client::session::{Block, Phase, QuestionState};
use gewu_protocol::{
    ContentPart, DirectoryEntry, FileEntry, ModelSummary, PROVIDER_DEFAULT_EFFORT,
    REASONING_EFFORTS, WorkspaceSummary,
};
use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use serde_json::Value;

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
/// MIME types drawn inline in a tool card. Raster only: an `<img>` with one
/// of these cannot execute script, and the list matches what `read_file`
/// emits after normalization.
const INLINE_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Base64 cap for inline rendering. Normalization keeps images under 5 MiB;
/// this protects transcript snapshots when the server limit is raised.
const MAX_INLINE_IMAGE_BYTES: usize = 4 * 1024 * 1024;

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
    let len = tab.model.blocks.len();
    if len == 0 {
        return "<div class=\"empty\">connected — type a message and press Enter</div>".to_string();
    }

    let start = tab.transcript_start.min(len - 1);
    let mut out = String::new();
    if start > 0 {
        write_load_earlier(&mut out, tab);
    }
    render_transcript_range(&mut out, tab, start, len);
    out
}

/// Render the older page `[start, end)` for prepending above the loaded
/// transcript.
pub fn transcript_prepend_html(tab: &Tab, start: usize, end: usize) -> String {
    let len = tab.model.blocks.len();
    let start = start.min(len);
    let end = end.clamp(start, len);
    let mut out = String::new();
    if start > 0 {
        write_load_earlier(&mut out, tab);
    }
    render_transcript_range(&mut out, tab, start, end);
    out
}

/// The sentinel that also serves as the auto-load trigger in the browser.
fn write_load_earlier(out: &mut String, tab: &Tab) {
    let _ = writeln!(
        out,
        "<div class=\"load-earlier\"><button type=\"button\" class=\"load-earlier-button\" data-load-earlier=\"{}\">Load earlier messages</button></div>",
        tab.id
    );
}

/// Render blocks `[start, end)` of a tab.
fn render_transcript_range(out: &mut String, tab: &Tab, start: usize, end: usize) {
    let last = tab.model.blocks.len().saturating_sub(1);
    let mut index = start;
    while index < end {
        if matches!(tab.model.blocks[index], Block::Tool { .. }) {
            let run_len = tool_run_len(&tab.model.blocks, index, end);
            render_tool_run(out, tab, &tab.model.blocks[index..index + run_len], index);
            index += run_len;
        } else {
            let streaming = tab.busy() && index == last;
            render_block(out, tab, &tab.model.blocks[index], index, streaming);
            index += 1;
        }
    }
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
        Block::User(_) | Block::UserMedia { .. } => "user",
        Block::Assistant(_) => "assistant",
        Block::Thought { .. } => "thought",
        Block::Error(_) => "error",
        Block::Info(_) => "info",
        Block::Compaction { .. } => unreachable!(),
        Block::Tool { .. } => return,
    };
    let _ = writeln!(out, "<div class=\"block {class}\" data-block=\"{index}\">");
    write_block_body(out, tab, block, index, streaming);
    out.push_str("</div>\n");
}

/// The message-actions trigger and its menu, written inside the message
/// bubble it belongs to (`.body.plain` is the positioning context, so the
/// trigger anchors to the message rather than to the whole block — which a
/// media attachment can widen).
fn message_menu_html(index: usize) -> String {
    format!(
        "<button type=\"button\" class=\"message-menu-trigger\" aria-label=\"Message actions\" aria-haspopup=\"menu\" aria-expanded=\"false\" aria-controls=\"message-menu-{index}\"></button><div id=\"message-menu-{index}\" class=\"message-menu\" role=\"menu\" hidden><button type=\"button\" role=\"menuitem\" class=\"chat-revert\" data-revert-block=\"{index}\" title=\"Revert conversation from here; files are not undone\">Revert chat</button></div>"
    )
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
        Block::User(text) | Block::UserMedia { text, .. } => {
            // A message submitted mid-run is queued into the running turn
            // until the agent's `steered` event confirms it was ingested;
            // while it sits in `pending_steers`, show that state.
            let queued = tab
                .pending_steers
                .iter()
                .any(|(pending, at)| *at == index && pending == text);
            let (prompt, plugin_context) = text
                .rsplit_once("\n\n[Plugin selection context: untrusted view data]\n")
                .and_then(|(prompt, context)| {
                    serde_json::from_str::<Vec<Value>>(context)
                        .ok()
                        .map(|c| (prompt, Some(c)))
                })
                .unwrap_or((text.as_str(), None));
            let with_media = if let Block::UserMedia { media, .. } = block {
                let _ = write!(
                    out,
                    "<div class=\"user-media-group\">{}",
                    user_media_html(media)
                );
                true
            } else {
                false
            };
            let _ = writeln!(
                out,
                "<div class=\"body plain\">{}{}</div>",
                escape(prompt),
                if queued {
                    format!(
                        "<span class=\"queue-status\">queued</span>{}",
                        message_menu_html(index)
                    )
                } else {
                    message_menu_html(index)
                }
            );
            if let Some(items) = plugin_context {
                let _ = writeln!(
                    out,
                    "<details class=\"plugin-context-chip\"><summary>App context · {}</summary><p class=\"plugin-context-note\">View context sent with this message. It is not a plugin command.</p><pre>{}</pre></details>",
                    items.len(),
                    escape(
                        &items
                            .iter()
                            .map(|item| {
                                item.as_str().map(str::to_owned).unwrap_or_else(|| {
                                    serde_json::to_string_pretty(item).unwrap_or_default()
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                );
            }
            if with_media {
                out.push_str("</div>\n");
            }
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

/// Inline markup for a user message's attachments: raster images as
/// thumbnails (the tool-media allowlist), other files as chips with their
/// name and MIME type.
fn user_media_html(media: &[ContentPart]) -> String {
    let mut out = String::new();
    if let Some(images) = tool_media_html(
        &media
            .iter()
            .filter(|part| matches!(part, ContentPart::Image { .. }))
            .cloned()
            .collect::<Vec<_>>(),
        "attachment",
    ) {
        out.push_str(&images);
    }
    for part in media {
        if let ContentPart::File { mime_type, .. } = part {
            let _ = writeln!(
                out,
                "<p class=\"attachment-chip\" data-mime=\"{}\">attached file ({})</p>",
                escape(mime_type),
                escape(mime_type)
            );
        }
    }
    out
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
    let run_len = tool_run_len(&tab.model.blocks, first, tab.model.blocks.len());
    let mut out = String::new();
    render_tool_run(
        &mut out,
        tab,
        &tab.model.blocks[first..first + run_len],
        first,
    );
    Some(out)
}

/// Whether `next` continues the tool run started by `prev`. Skill loads and
/// questions are standalone cards so their content stays visible instead of
/// folding into an "Explored" group.
pub(crate) fn same_tool_run(prev: &Block, next: &Block) -> bool {
    match (prev, next) {
        (Block::Tool { tool: prev, .. }, Block::Tool { tool: next, .. }) => {
            prev != "skill" && next != "skill" && prev != "question" && next != "question"
        }
        _ => false,
    }
}

/// Number of blocks rendered as one tool card, starting at the tool block
/// `first` and clamped to `end`.
pub(crate) fn tool_run_len(blocks: &[Block], first: usize, end: usize) -> usize {
    let end = end.min(blocks.len());
    if first >= end || !matches!(blocks[first], Block::Tool { .. }) {
        return 0;
    }
    let mut len = 1;
    while first + len < end && same_tool_run(&blocks[first + len - 1], &blocks[first + len]) {
        len += 1;
    }
    len
}

/// Render the interactive form (or its settled summary) for a `question`
/// tool block. Questions are tabs; one Submit applies every tab's answer.
fn render_question_card(out: &mut String, tab: &Tab, state: &QuestionState, index: usize) {
    let questions = state.questions();
    let title = match questions {
        [question] => short_header(&question.header),
        _ => format!("{} questions", questions.len()),
    };
    let _ = writeln!(
        out,
        "<div class=\"tool question depth-0\" data-block=\"{index}\">\
         <div class=\"question-head\"><span class=\"tool-name\">question</span>\
         <span class=\"question-title\">{}</span></div>",
        escape(&title)
    );
    match state {
        QuestionState::Pending { request_id, .. } => {
            render_question_form(out, tab, request_id, questions, index);
        }
        QuestionState::Answered { answers, .. } => {
            render_question_answers(out, questions, answers);
        }
        QuestionState::Dismissed { .. } => {
            out.push_str("<div class=\"question-dismissed\">Dismissed</div>\n");
        }
    }
    out.push_str("</div>\n");
}

fn short_header(header: &str) -> String {
    let header = header.trim();
    let mut label: String = header.chars().take(30).collect();
    if header.chars().count() > 30 {
        label.push('…');
    }
    label
}

fn render_question_form(
    out: &mut String,
    tab: &Tab,
    request_id: &str,
    questions: &[gewu_protocol::Question],
    index: usize,
) {
    let _ = writeln!(
        out,
        "<form class=\"question-form\" data-tab=\"{}\" data-request=\"{}\" data-block=\"{index}\">",
        tab.id,
        escape(request_id)
    );
    // One question needs no tab strip: the card title already names it.
    if questions.len() > 1 {
        out.push_str("<div class=\"question-tabs\" role=\"tablist\">");
        for (number, question) in questions.iter().enumerate() {
            let active = number == 0;
            let _ = write!(
                out,
                "<button type=\"button\" role=\"tab\" class=\"question-tab{}\" data-qtab=\"{number}\" \
                 aria-selected=\"{}\">{}</button>",
                if active { " active" } else { "" },
                active,
                escape(&short_header(&question.header))
            );
        }
        out.push_str("</div>\n");
    }

    for (number, question) in questions.iter().enumerate() {
        let active = number == 0;
        let _ = writeln!(
            out,
            "<div class=\"question-panel{}\" data-qpanel=\"{number}\" role=\"tabpanel\"{}>",
            if active { " active" } else { "" },
            if active { "" } else { " hidden" }
        );
        let _ = writeln!(
            out,
            "<p class=\"question-text\">{}</p>",
            escape(question.question.trim())
        );
        let kind = if question.multiple {
            "checkbox"
        } else {
            "radio"
        };
        let name = format!("q-{}-{number}", escape(request_id));
        for option in &question.options {
            let label = option.label.trim();
            let _ = write!(
                out,
                "<label class=\"question-option\"><input type=\"{kind}\" name=\"{name}\" \
                 value=\"{}\"><span class=\"question-option-text\"><span class=\"question-label\">{}</span>",
                escape(label),
                escape(label)
            );
            let description = option.description.trim();
            if !description.is_empty() {
                let _ = write!(
                    out,
                    "<span class=\"question-desc\">{}</span>",
                    escape(description)
                );
            }
            out.push_str("</span></label>");
        }
        if question.custom {
            let _ = writeln!(
                out,
                "<label class=\"question-custom-row\"><span class=\"question-custom-label\">Type your own answer</span>\
                 <input type=\"text\" class=\"question-custom\" data-custom=\"{number}\" autocomplete=\"off\"></label>"
            );
        }
        out.push_str("</div>\n");
    }
    out.push_str(
        "<div class=\"question-actions\"><button type=\"submit\" class=\"question-submit\">Submit</button>\
         <button type=\"button\" class=\"question-dismiss\" data-question-dismiss>Dismiss</button></div>\
         </form>\n",
    );
}

fn render_question_answers(
    out: &mut String,
    questions: &[gewu_protocol::Question],
    answers: &[Vec<String>],
) {
    out.push_str("<div class=\"question-answers\">");
    for (number, question) in questions.iter().enumerate() {
        let answer = answers.get(number).map(Vec::as_slice).unwrap_or_default();
        let answer = if answer.is_empty() {
            "Unanswered".to_string()
        } else {
            answer.join(", ")
        };
        let _ = write!(
            out,
            "<div class=\"question-answer\"><span class=\"question-answer-q\">{}</span>\
             <span class=\"question-answer-a\">{}</span></div>",
            escape(question.question.trim()),
            escape(&answer)
        );
    }
    out.push_str("</div>\n");
}

/// Render a run of consecutive tool blocks inside one card.
fn render_tool_run(out: &mut String, tab: &Tab, run: &[Block], first: usize) {
    let tools: Vec<ToolView> = run
        .iter()
        .filter_map(|block| match block {
            Block::Tool {
                tool,
                input,
                output,
                images,
                status,
                question,
            } => Some(ToolView {
                tool,
                input,
                output: output.as_deref(),
                status: status.as_deref(),
                images,
                question: question.as_ref(),
            }),
            _ => None,
        })
        .collect();

    let card_class = if is_question_run(run) {
        "tool-card question-card"
    } else {
        "tool-card"
    };
    let _ = writeln!(out, "<div class=\"{card_class}\" data-card=\"{first}\">");
    if tools.len() == 1 {
        render_tool(out, tab, &tools[0], first, 0);
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
            for (offset, view) in tools.iter().enumerate() {
                render_tool(out, tab, view, first + offset, 1);
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

/// Whether a tool run is the standalone `question` card. The card's rail and
/// bezel live on the wrapper, so the wrapper needs its own accent class.
pub(crate) fn is_question_run(run: &[Block]) -> bool {
    matches!(run, [Block::Tool { tool, .. }] if tool == "question")
}

/// One tool row's render inputs.
struct ToolView<'a> {
    tool: &'a str,
    input: &'a str,
    output: Option<&'a str>,
    status: Option<&'a str>,
    images: &'a [ContentPart],
    question: Option<&'a QuestionState>,
}

/// One tool row plus its output.
fn render_tool(out: &mut String, tab: &Tab, view: &ToolView, index: usize, depth: usize) {
    let ToolView {
        tool,
        input,
        output,
        status,
        images,
        question,
    } = view;
    if let Some(question) = question {
        render_question_card(out, tab, question, index);
        return;
    }
    let (tool, input, output, status) = (*tool, *input, *output, *status);
    let (name, title) = tool_row(tool, input);
    let running = output.is_none();
    let expanded = tab.expanded_outputs.contains(&index);
    let _ = writeln!(
        out,
        "<div class=\"tool{} depth-{depth}\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">\
         <button class=\"tool-row\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">\
         <span class=\"tool-name\">{}</span>{} <span class=\"tool-title\">{}</span>{}\
         {}</button>",
        if running { " running" } else { "" },
        tab.id,
        tab.id,
        escape(name),
        if running {
            "<span class=\"tool-spinner\" aria-hidden=\"true\"></span>"
        } else {
            ""
        },
        escape(&title),
        if has_inline_image(images) {
            " <span class=\"tool-badge\">image</span>"
        } else {
            ""
        },
        match (running, status) {
            (true, Some(status)) => {
                format!(" <span class=\"tool-status\">{}</span>", escape(status))
            }
            _ => String::new(),
        }
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
    let output = if (tool == "plugin_call" || tool.starts_with("plg_"))
        && let Some((text, id)) = output.rsplit_once("\n[GeWu plugin result: ")
        && let Some(id) = id.strip_suffix(']')
        && uuid::Uuid::parse_str(id).is_ok()
    {
        let _ = writeln!(
            out,
            "<button type=\"button\" class=\"tool-more\" data-plugin-result=\"{}\">Open application result</button>",
            escape(id)
        );
        text
    } else {
        output
    };
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let shown = if expanded {
        lines.len().min(TOOL_MAX_LINES)
    } else {
        lines.len().min(TOOL_PREVIEW_LINES)
    };
    if shown > 0 {
        let _ = writeln!(
            out,
            "<pre class=\"tool-output\">{}</pre>",
            escape(&lines[..shown].join("\n"))
        );
    }
    if lines.len() > shown {
        let hidden = lines.len() - shown;
        let _ = writeln!(
            out,
            "<button class=\"tool-more\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">… +{hidden} line{}</button>",
            tab.id,
            if hidden == 1 { "" } else { "s" }
        );
    }
    // Images stay collapsed with the rest of the output: the row's badge says
    // one is attached, and expanding shows it.
    if expanded && let Some(media) = tool_media_html(images, &title) {
        out.push_str(&media);
    }
    out.push_str("</div>\n");
}

/// Whether any image part is a raster format the GUI draws inline.
fn has_inline_image(images: &[ContentPart]) -> bool {
    images.iter().any(|part| {
        matches!(part, ContentPart::Image { mime_type, .. }
            if INLINE_IMAGE_MIMES.contains(&mime_type.as_str()))
    })
}

/// Inline `<img>` markup for a tool's image parts, or `None` when nothing is
/// renderable.
///
/// Only the [`INLINE_IMAGE_MIMES`] allowlist is drawn: those raster formats
/// cannot execute script when loaded through `<img>`. Payloads over
/// [`MAX_INLINE_IMAGE_BYTES`] fall back to a one-line note so a raised server
/// limit cannot bloat transcript snapshots.
fn tool_media_html(images: &[ContentPart], alt: &str) -> Option<String> {
    let mut out = String::new();
    for part in images {
        let ContentPart::Image { data, mime_type } = part else {
            continue;
        };
        if !INLINE_IMAGE_MIMES.contains(&mime_type.as_str()) {
            continue;
        }
        if data.len() > MAX_INLINE_IMAGE_BYTES {
            let _ = writeln!(
                out,
                "<p class=\"tool-media-note\">image attached ({:.1} MB) — too large to display</p>",
                data.len() as f64 / (1024.0 * 1024.0)
            );
            continue;
        }
        let _ = writeln!(
            out,
            "<figure class=\"tool-media\"><img class=\"tool-image\" src=\"data:{};base64,{}\" alt=\"{}\" loading=\"lazy\" decoding=\"async\"></figure>",
            escape(mime_type),
            escape(data),
            escape(alt)
        );
    }
    (!out.is_empty()).then_some(out)
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
        "workflow_cancel" => "wf·cancel",
        other => other,
    }
}

fn tool_row<'a>(tool: &'a str, input: &str) -> (&'a str, String) {
    let title = match tool {
        "workflow_start" => crate::workflow::spec_title(input).unwrap_or_else(|| input.to_string()),
        // Status/cancel inputs are already just the run_id.
        "workflow_status" | "workflow_cancel" => input.to_string(),
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

fn selected_model<'a>(tab: &Tab, models: &'a [ModelSummary]) -> Option<&'a ModelSummary> {
    match &tab.model_selector {
        Some(selector) => models.iter().find(|model| &model.selector == selector),
        None => models
            .iter()
            .find(|model| Some(&model.label) == tab.model_label.as_ref()),
    }
}

/// Whether an effort can be sent for `model`; mirrors the server's
/// `ModelEntry::supports_effort`.
fn selectable_effort(model: &ModelSummary, effort: &str) -> bool {
    effort == PROVIDER_DEFAULT_EFFORT
        || REASONING_EFFORTS.contains(&effort)
        || model.default_effort.as_deref() == Some(effort)
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
        let selected = selected_model(tab, models).map(|model| model.selector.as_str());
        let _ = write!(
            out,
            "<label class=\"kv\"><select class=\"model-picker\" data-model-picker data-tab=\"{}\" aria-label=\"switch model\" title=\"Switch model\"{}>",
            tab.id,
            if tab.busy() { " disabled" } else { "" }
        );
        for model in models {
            let _ = write!(
                out,
                "<option value=\"{}\"{}>{}</option>",
                escape(&model.selector),
                if selected == Some(model.selector.as_str()) {
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
            "<span class=\"kv\"><span class=\"v model\" title=\"Model\">{}</span></span>",
            escape(label)
        );
    }
    if let Some(model) = selected_model(tab, models) {
        let disabled = tab.busy() || !model.supports_reasoning_effort;
        let selected = tab
            .reasoning_effort
            .as_deref()
            .filter(|effort| selectable_effort(model, effort))
            .or(model.default_effort.as_deref())
            .unwrap_or(PROVIDER_DEFAULT_EFFORT);
        let _ = write!(
            out,
            "<label class=\"kv\"><select class=\"effort-picker\" data-effort-picker data-tab=\"{}\" aria-label=\"reasoning effort\" title=\"Reasoning effort\"{}>",
            tab.id,
            if disabled { " disabled" } else { "" }
        );
        let _ = write!(
            out,
            "<option value=\"{PROVIDER_DEFAULT_EFFORT}\" title=\"provider default\"{}>default</option>",
            if selected == PROVIDER_DEFAULT_EFFORT {
                " selected"
            } else {
                ""
            }
        );
        for effort in REASONING_EFFORTS {
            let _ = write!(
                out,
                "<option value=\"{effort}\"{}>{effort}</option>",
                if selected == *effort { " selected" } else { "" }
            );
        }
        // A configured effort outside the known list stays selectable, so
        // choosing the provider default can be undone.
        if let Some(effort) = &model.default_effort
            && !REASONING_EFFORTS.contains(&effort.as_str())
            && effort != PROVIDER_DEFAULT_EFFORT
        {
            let _ = write!(
                out,
                "<option value=\"{}\" title=\"configured default\"{}>{}</option>",
                escape(effort),
                if selected == effort { " selected" } else { "" },
                escape(effort)
            );
        }
        out.push_str("</select></label>");
    }
    match (&tab.model.phase, tab.model.status.as_deref()) {
        (Phase::Idle, _) => out.push_str("<span class=\"phase\"><i class=\"dot\"></i>idle</span>"),
        // A pending `question` form blocks the turn on the user: show it as a
        // distinct phase instead of "thinking…".
        (Phase::Busy, _) if tab.model.has_pending_question() => {
            out.push_str("<span class=\"phase waiting\"><i class=\"dot\"></i>waiting…</span>");
        }
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
        "<div class=\"sidebar-inner\"><div class=\"sidebar-head\"><div class=\"sidebar-tabs\" role=\"tablist\" aria-label=\"Sidebar views\"><button type=\"button\" class=\"sidebar-tab active\" role=\"tab\" aria-selected=\"true\" aria-controls=\"sidebar-files\" data-sidebar-tab=\"files\">FILES</button><button type=\"button\" class=\"sidebar-tab\" role=\"tab\" aria-selected=\"false\" aria-controls=\"sidebar-tools\" data-sidebar-tab=\"tools\">TOOLS</button><button type=\"button\" class=\"sidebar-tab\" role=\"tab\" aria-selected=\"false\" aria-controls=\"sidebar-preview\" data-sidebar-tab=\"preview\">PREVIEW</button></div><button type=\"button\" class=\"sidebar-collapse\" data-sidebar-collapse title=\"Hide files\" aria-label=\"Hide files\">›</button></div><div id=\"sidebar-files\" class=\"sidebar-panel active\" role=\"tabpanel\">{files_html}</div><div id=\"sidebar-tools\" class=\"sidebar-panel\" role=\"tabpanel\" hidden>{tools_html}</div><div id=\"sidebar-preview\" class=\"sidebar-panel\" role=\"tabpanel\" hidden></div></div>"
    )
}

/// Render the left session manager: workspace groups with their sessions.
///
/// `version` is the running app's version, shown next to the brand.
/// Render the session sidebar, with an optional newest-release badge.
pub fn session_sidebar_html_with_update(
    tabs: &[TabInfo],
    workspaces: &[WorkspaceSummary],
    active: u64,
    version: &str,
    latest: Option<&str>,
) -> String {
    let mut workspace_ids: Vec<String> = workspaces
        .iter()
        .map(|workspace| workspace.id.clone())
        .collect();
    for tab in tabs {
        if !workspace_ids.iter().any(|id| id == &tab.workspace) {
            workspace_ids.push(tab.workspace.clone());
        }
    }

    let update = latest
        .map(|latest| {
            format!(
                "<div class=\"session-update\" role=\"status\" aria-live=\"polite\" \
                 title=\"Run `gewu update` in a terminal, then restart GeWu.\">\
                 <span class=\"session-update-icon\" aria-hidden=\"true\">↑</span>\
                 <span>Update available: v{}</span></div>",
                escape(latest)
            )
        })
        .unwrap_or_default();

    let mut out = format!(
        "<aside class=\"session-sidebar\" aria-label=\"Sessions by workspace\"><header class=\"session-sidebar-header\"><span class=\"brand-mark session-brand\" title=\"GeWu agent\"><svg class=\"brand-icon\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.7\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><rect x=\"3.5\" y=\"3.5\" width=\"17\" height=\"17\" rx=\"3.5\"/><circle cx=\"12\" cy=\"12\" r=\"4.25\"/><path d=\"M12 3.5v4.25M12 16.25v4.25M3.5 12h4.25M16.25 12h4.25\"/></svg></span><span class=\"session-sidebar-title\">GeWu <span class=\"session-sidebar-version\">v{}</span></span><button type=\"button\" class=\"session-collapse\" data-sessions-collapse title=\"Close sidebar\" aria-label=\"Close sidebar\"><svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><rect x=\"3\" y=\"4\" width=\"18\" height=\"16\" rx=\"2\"/><path d=\"M9 4v16\"/><path d=\"m16 15-3-3 3-3\"/></svg></button></header>{update}<button type=\"button\" class=\"session-new-primary\" data-session-new title=\"Open workspace\" aria-label=\"Open workspace\"><svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.7\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><path d=\"M3.5 6.5A1.5 1.5 0 0 1 5 5h4l2 2h8a1.5 1.5 0 0 1 1.5 1.5V10\"/><path d=\"M3.5 10.5h17l-2 8h-13l-2-8Z\"/></svg><span>Open workspace</span></button><div class=\"session-section-label\">WORKSPACE</div><div class=\"session-workspace-groups\">",
        escape(version)
    );
    for (group_index, workspace_id) in workspace_ids.into_iter().enumerate() {
        let summary = workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id);
        let path = summary
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| {
                workspace_id
                    .strip_prefix("runtime:")
                    .unwrap_or(&workspace_id)
                    .to_string()
            });
        let label = summary
            .map(workspace_label)
            .unwrap_or_else(|| workspace_id.clone());
        let list_id = format!("session-list-{group_index}");
        let menu_id = format!("session-workspace-menu-{group_index}");
        let _ = write!(
            out,
            "<section class=\"session-workspace-group\" data-session-workspace-group=\"{}\"><header class=\"session-workspace-header\"><button type=\"button\" class=\"session-workspace-toggle\" data-session-workspace-toggle=\"{}\" aria-expanded=\"true\" aria-controls=\"{list_id}\"><span class=\"session-workspace-caret\" aria-hidden=\"true\"></span><span class=\"session-workspace-heading\"><strong>{}</strong><code title=\"{}\">{}</code></span></button><button type=\"button\" class=\"session-workspace-menu-trigger\" data-session-workspace-menu=\"{}\" aria-haspopup=\"menu\" aria-expanded=\"false\" aria-controls=\"{menu_id}\" title=\"Workspace actions\" aria-label=\"Workspace actions for {}\">⋯</button><div id=\"{menu_id}\" class=\"session-workspace-menu\" role=\"menu\" hidden><button type=\"button\" role=\"menuitem\" class=\"session-workspace-close\" data-session-workspace-close=\"{}\" title=\"Close workspace; sessions are kept\" aria-label=\"Close workspace {}\">Close workspace</button></div></header><div class=\"session-list\" id=\"{list_id}\">",
            escape(&workspace_id),
            escape(&workspace_id),
            escape(&label),
            escape(&path),
            escape(&path),
            escape(&workspace_id),
            escape(&label),
            escape(&workspace_id),
            escape(&label),
        );
        let group_tabs: Vec<&TabInfo> = tabs
            .iter()
            .filter(|tab| tab.workspace == workspace_id)
            .collect();
        for tab in group_tabs {
            let active_class = if tab.id == active { " active" } else { "" };
            let busy = if tab.busy { " busy" } else { "" };
            let waiting = if tab.waiting { " waiting" } else { "" };
            let spinner = if tab.busy {
                format!(
                    "<span class=\"session-spinner\" aria-label=\"{}\"></span>",
                    if tab.waiting {
                        "waiting for input"
                    } else {
                        "working"
                    }
                )
            } else {
                String::new()
            };
            let _ = write!(
                out,
                "<div class=\"session-row{active_class}{busy}{waiting}\"><button type=\"button\" class=\"session-select\" data-session-tab=\"{}\" title=\"{}\"><span class=\"session-label\">{}</span>{}</button><button type=\"button\" class=\"session-close\" data-session-close=\"{}\" title=\"Close session\" aria-label=\"Close {}\">×</button></div>",
                tab.id,
                escape(&tab.label),
                escape(&tab.label),
                spinner,
                tab.id,
                escape(&tab.label),
            );
        }
        let _ = write!(
            out,
            "<button type=\"button\" class=\"session-add\" data-session-new-workspace=\"{}\" data-session-new-path=\"{}\" title=\"New session in {}\" aria-label=\"New session in {}\"><span class=\"session-add-glyph\" aria-hidden=\"true\">＋</span><span>New session</span></button>",
            escape(&workspace_id),
            escape(&path),
            escape(&label),
            escape(&label),
        );
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
    _workspaces: &[WorkspaceSummary],
    _active_workspace: &str,
    expanded: &BTreeSet<String>,
    cache: &BTreeMap<String, Vec<FileEntry>>,
) -> String {
    let root_label = root.unwrap_or("files");
    let mut out =
        String::from("<section class=\"side-section file-tree-section\" id=\"file-tree\">");
    let _ = write!(
        out,
        "<div class=\"tree-toolbar\"><p class=\"tree-root\" title=\"{}\">{}</p><span class=\"tree-actions\"><button class=\"tree-upload\" type=\"button\" data-tree-upload title=\"Upload files\" aria-label=\"Upload files\">⬆</button><button class=\"tree-refresh\" type=\"button\" data-tree-refresh title=\"Refresh files\" aria-label=\"Refresh files\">↻</button></span></div><ul class=\"tree-list\">",
        escape(root_label),
        escape(root_label),
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
        "<p class=\"workspace-browser-path\" aria-label=\"Current directory\" title=\"{}\">{}</p><div class=\"workspace-browser-actions\">",
        escape(path),
        escape(path)
    );
    if let Some(parent) = parent {
        let _ = write!(
            out,
            "<button type=\"button\" data-workspace-browse-path=\"{}\">← Parent directory</button>",
            escape(parent)
        );
    }
    let _ = write!(
        out,
        "<button type=\"button\" data-workspace-add-path=\"{}\">Open this directory</button></div><ul class=\"workspace-browser-list\">",
        escape(path)
    );
    for entry in entries {
        let _ = write!(
            out,
            "<li><button type=\"button\" data-workspace-browse-path=\"{}\"><span class=\"workspace-folder-icon\" aria-hidden=\"true\"></span><span class=\"workspace-directory-name\">{}</span><span class=\"workspace-row-arrow\" aria-hidden=\"true\">›</span></button></li>",
            escape(&entry.path),
            escape(&entry.name)
        );
    }
    if entries.is_empty() {
        out.push_str(
            "<li class=\"workspace-browser-empty\">No visible folders in this directory</li>",
        );
    }
    out.push_str("</ul></div></section>");
    out
}

/// Render the workdir choice shown before a new session tab is created.
pub fn session_workspace_picker_html(workspaces: &[WorkspaceSummary], browse_path: &str) -> String {
    let mut out = String::from(
        "<section id=\"session-workspace-picker\" class=\"workspace-browser\" role=\"dialog\" aria-label=\"Open workspace\"><div class=\"workspace-browser-card session-picker-card\"><header><strong>Open workspace</strong><button type=\"button\" data-new-session-close aria-label=\"Close\">×</button></header><p class=\"workspace-picker-note\">Choose a workspace or browse for a folder on the server.</p><ul class=\"workspace-browser-list session-workspace-list\">",
    );
    for workspace in workspaces {
        let _ = write!(
            out,
            "<li><button type=\"button\" class=\"session-workspace-option\" data-new-session-create data-new-session-workspace=\"{}\" data-new-session-path=\"{}\"><span class=\"workspace-folder-icon\" aria-hidden=\"true\"></span><span class=\"session-workspace-details\"><span class=\"session-workspace-name\">{}</span><code>{}</code></span><span class=\"workspace-row-arrow\" aria-hidden=\"true\">→</span></button></li>",
            escape(&workspace.id),
            escape(&workspace.path),
            escape(&workspace_label(workspace)),
            escape(&workspace.path),
        );
    }
    let _ = write!(
        out,
        "<li><button type=\"button\" class=\"session-workspace-browse\" data-new-session-browse-path=\"{}\">Browse server directories →</button></li></ul></div></section>",
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
        "<section id=\"session-workspace-picker\" class=\"workspace-browser\" role=\"dialog\" aria-label=\"Open workspace\"><div class=\"workspace-browser-card session-picker-card\"><header><strong>Open workspace</strong><button type=\"button\" data-new-session-close aria-label=\"Close\">×</button></header>",
    );
    let _ = write!(
        out,
        "<p class=\"workspace-browser-path\" aria-label=\"Current directory\" title=\"{}\">{}</p><div class=\"workspace-browser-actions\">",
        escape(path),
        escape(path)
    );
    if let Some(parent) = parent {
        let _ = write!(
            out,
            "<button type=\"button\" data-new-session-browse-path=\"{}\">← Parent directory</button>",
            escape(parent)
        );
    }
    let workspace_id = format!("runtime:{path}");
    let _ = write!(
        out,
        "<button type=\"button\" class=\"workspace-browser-primary\" data-new-session-create data-new-session-workspace=\"{}\" data-new-session-path=\"{}\">Open this directory</button></div><ul class=\"workspace-browser-list\">",
        escape(&workspace_id),
        escape(path)
    );
    for entry in entries {
        let _ = write!(
            out,
            "<li><button type=\"button\" data-new-session-browse-path=\"{}\"><span class=\"workspace-folder-icon\" aria-hidden=\"true\"></span><span class=\"workspace-directory-name\">{}</span><span class=\"workspace-row-arrow\" aria-hidden=\"true\">›</span></button></li>",
            escape(&entry.path),
            escape(&entry.name)
        );
    }
    if entries.is_empty() {
        out.push_str(
            "<li class=\"workspace-browser-empty\">No visible folders in this directory</li>",
        );
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
    use crate::workspace::{TRANSCRIPT_WINDOW_BLOCKS, Workspace};
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
    fn user_media_renders_thumbnails_and_file_chips() {
        let (ws, id) = tab_with_blocks(vec![Block::UserMedia {
            text: "describe this".into(),
            media: vec![
                ContentPart::Image {
                    data: "aGk=".into(),
                    mime_type: "image/png".into(),
                },
                ContentPart::File {
                    data: "aGk=".into(),
                    mime_type: "application/pdf".into(),
                },
            ],
        }]);
        let html = transcript_html(ws.tab(id).unwrap());

        assert!(html.contains("describe this"), "{html}");
        assert!(
            html.contains("data:image/png;base64,aGk="),
            "inline image must render as a thumbnail: {html}"
        );
        assert!(
            html.contains("attachment-chip"),
            "non-media attachments must render as chips: {html}"
        );
        // Attachments sit above the message text, inside one column group.
        let group_at = html.find("<div class=\"user-media-group\">").unwrap();
        let media_at = html.find("data:image/png;base64,aGk=").unwrap();
        let text_at = html.find("describe this").unwrap();
        assert!(
            group_at < media_at && media_at < text_at,
            "attachments must render above the message inside the group: {html}"
        );
        // The message-menu trigger rides inside the bubble (after the text),
        // so it anchors to the message rather than the image above it.
        let trigger_at = html.find("message-menu-trigger").unwrap();
        assert!(
            text_at < trigger_at,
            "the message-menu trigger must anchor to the bubble, not the attachment: {html}"
        );
        assert!(
            !html.contains("aGk=\"") || html.contains("base64,"),
            "base64 must never render as text: {html}"
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
    fn file_tree_keeps_active_root_without_workspace_selector() {
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
        assert!(html.contains("/tmp/frontend"), "{html}");
        assert!(!html.contains("data-workspace-select"), "{html}");
        assert!(!html.contains("ROOT"), "{html}");
    }

    #[test]
    fn file_toolbar_keeps_file_actions_without_directory_chooser() {
        let html = file_tree_with_workspaces_html(
            Some("/tmp/project"),
            &[],
            "cwd",
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert!(!html.contains("data-workspace-add"), "{html}");
        assert!(!html.contains("data-workspace-browse-path"), "{html}");
        assert!(html.contains("data-tree-upload"), "{html}");
        assert!(html.contains("data-tree-refresh"), "{html}");
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
        assert!(
            html.contains("data-workspace-browse-path=\"/tmp/project\""),
            "{html}"
        );
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
        assert!(html.contains("<strong>Open workspace</strong>"), "{html}");
        assert!(html.contains("aria-label=\"Open workspace\""), "{html}");
        assert!(
            html.contains("data-new-session-workspace=\"tachyon\""),
            "{html}"
        );
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
        assert!(html.contains("<strong>Open workspace</strong>"), "{html}");
        assert!(
            html.contains(
                "data-new-session-browse-path=\"/data3/litian/projects/tachyon_project\""
            ),
            "{html}"
        );
    }

    #[test]
    fn session_sidebar_groups_sessions_by_workspace() {
        let html = session_sidebar_html_with_update(
            &[
                TabInfo {
                    id: 1,
                    label: "tachyon work".into(),
                    busy: false,
                    waiting: false,
                    session_id: Some("session-1".into()),
                    workspace: "tachyon".into(),
                },
                TabInfo {
                    id: 2,
                    label: "wiki notes".into(),
                    busy: true,
                    waiting: true,
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
            "9.9.9-test",
            None,
        );
        assert!(html.contains("class=\"session-sidebar\""), "{html}");
        assert!(html.contains("session-brand"), "{html}");
        assert!(html.contains("class=\"brand-icon\""), "{html}");
        assert!(!html.contains("格物"), "{html}");
        assert!(
            html.contains(
                "session-sidebar-title\">GeWu <span class=\"session-sidebar-version\">v9.9.9-test</span></span>"
            ),
            "{html}"
        );
        assert!(html.contains("session-new-primary"), "{html}");
        assert!(
            html.contains("title=\"Open workspace\" aria-label=\"Open workspace\""),
            "{html}"
        );
        assert!(html.contains("<span>Open workspace</span>"), "{html}");
        assert!(
            html.contains("M3.5 6.5A1.5 1.5 0 0 1 5 5h4l2 2h8"),
            "{html}"
        );
        assert!(
            !html.contains("M4 20h4l10-10-4-4L4 16v4Z"),
            "pencil icon should be replaced: {html}"
        );
        assert!(
            html.contains("session-section-label\">WORKSPACE</div>"),
            "{html}"
        );
        assert!(!html.contains("RECENT"), "{html}");
        assert!(html.contains("data-session-new"), "{html}");
        assert!(html.contains("data-sessions-collapse"), "{html}");
        assert!(html.contains("data-session-tab=\"1\""), "{html}");
        assert!(html.contains("data-session-tab=\"2\""), "{html}");
        assert!(
            html.contains("data-session-new-workspace=\"tachyon\""),
            "{html}"
        );
        assert!(
            html.contains("data-session-workspace-group=\"tachyon\""),
            "{html}"
        );
        assert!(
            html.contains("data-session-workspace-toggle=\"tachyon\""),
            "{html}"
        );
        assert!(html.contains("aria-controls=\"session-list-0\""), "{html}");
        assert!(html.contains("id=\"session-list-0\""), "{html}");
        assert!(
            html.contains("data-session-workspace-toggle=\"wiki\""),
            "{html}"
        );
        assert!(html.contains("id=\"session-list-1\""), "{html}");
        assert!(html.contains("aria-expanded=\"true\""), "{html}");
        assert!(html.contains("session-row active"), "{html}");
        assert!(
            html.contains("class=\"session-row active busy waiting\""),
            "{html}"
        );
        assert!(
            html.contains(
                "<span class=\"session-spinner\" aria-label=\"waiting for input\"></span>"
            ),
            "{html}"
        );
        assert!(html.contains("data-session-close=\"2\""), "{html}");
        assert!(
            html.contains("class=\"session-add\" data-session-new-workspace=\"tachyon\""),
            "{html}"
        );
        assert!(
            html.contains(
                "class=\"session-workspace-menu-trigger\" data-session-workspace-menu=\"tachyon\" aria-haspopup=\"menu\" aria-expanded=\"false\" aria-controls=\"session-workspace-menu-0\""
            ),
            "{html}"
        );
        assert!(
            html.contains(
                "id=\"session-workspace-menu-0\" class=\"session-workspace-menu\" role=\"menu\" hidden"
            ),
            "{html}"
        );
        assert!(
            html.contains("data-session-workspace-close=\"tachyon\""),
            "{html}"
        );
        assert!(html.contains(">Close workspace</button>"), "{html}");
        assert!(
            html.contains("aria-controls=\"session-workspace-menu-1\""),
            "{html}"
        );
        assert!(!html.contains("class=\"session-new\""), "{html}");
        let list = html.find("id=\"session-list-0\"").expect("list rendered");
        let add = html
            .find("class=\"session-add\"")
            .expect("add row rendered");
        assert!(list < add, "{html}");
    }

    #[test]
    fn session_sidebar_shows_an_update_notice() {
        let html = session_sidebar_html_with_update(&[], &[], 0, "0.1.4", Some("0.1.5"));
        assert!(html.contains("class=\"session-update\""), "{html}");
        assert!(html.contains("Update available: v0.1.5"), "{html}");
        assert!(html.contains("Run `gewu update`"), "{html}");

        let escaped = session_sidebar_html_with_update(&[], &[], 0, "0.1.4", Some("<0.1.5>"));
        assert!(escaped.contains("v&lt;0.1.5&gt;"), "{escaped}");
        assert!(
            !session_sidebar_html_with_update(&[], &[], 0, "0.1.4", None)
                .contains("session-update")
        );
    }

    #[test]
    fn empty_workspace_group_still_has_disclosure() {
        let html = session_sidebar_html_with_update(
            &[],
            &[WorkspaceSummary {
                id: "cwd".into(),
                path: "/tmp/project".into(),
            }],
            0,
            "test",
            None,
        );
        assert!(
            html.contains("class=\"session-add\" data-session-new-workspace=\"cwd\""),
            "{html}"
        );
        assert!(!html.contains("No sessions"), "{html}");
        assert!(
            html.contains("data-session-workspace-toggle=\"cwd\""),
            "{html}"
        );
        assert!(html.contains("aria-expanded=\"true\""), "{html}");
        assert!(html.contains("id=\"session-list-0\""), "{html}");
    }

    #[test]
    fn workspace_close_is_accessible_and_attribute_escaped() {
        let html = session_sidebar_html_with_update(
            &[],
            &[WorkspaceSummary {
                id: "project\"<&".into(),
                path: "/tmp/project".into(),
            }],
            0,
            "test",
            None,
        );
        assert!(
            html.contains("data-session-workspace-menu=\"project&quot;&lt;&amp;\""),
            "{html}"
        );
        assert!(
            html.contains("aria-label=\"Workspace actions for project&quot;&lt;&amp;\""),
            "{html}"
        );
        assert!(
            html.contains("class=\"session-workspace-menu\" role=\"menu\" hidden"),
            "{html}"
        );
        assert!(html.contains("data-session-workspace-close=\"project&quot;&lt;&amp;\""));
        assert!(html.contains("aria-label=\"Close workspace project&quot;&lt;&amp;\""));
        assert!(html.contains("title=\"Close workspace; sessions are kept\""));
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
        assert!(html.contains("class=\"sidebar-inner\""), "{html}");
        assert!(html.contains("data-sidebar-collapse"), "{html}");
        assert!(!html.contains("<h2 class=\"side-title\">files"), "{html}");
        assert!(!html.contains("<h2 class=\"side-title\">tools"), "{html}");
        assert!(html.contains("data-tree-refresh"), "{html}");
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
    fn transcript_renders_only_the_loaded_window() {
        let blocks: Vec<Block> = (0..250)
            .map(|n| Block::User(format!("message {n}")))
            .collect();
        let (mut ws, id) = tab_with_blocks(blocks);
        let tab = ws.tab_mut(id).unwrap();
        tab.reset_transcript_window();
        let start = 250 - TRANSCRIPT_WINDOW_BLOCKS;

        let html = transcript_html(tab);

        assert!(html.contains(&format!("data-block=\"{start}\"")), "{html}");
        assert!(
            !html.contains(&format!("data-block=\"{}\"", start - 1)),
            "{html}"
        );
        assert!(html.contains("data-load-earlier"), "{html}");
        assert!(html.contains("Load earlier messages"), "{html}");
    }

    #[test]
    fn transcript_without_hidden_history_has_no_load_control() {
        let (ws, id) = tab_with_blocks(vec![Block::User("hello".into())]);
        let html = transcript_html(ws.tab(id).unwrap());

        assert!(!html.contains("load-earlier"), "{html}");
    }

    #[test]
    fn prepend_renders_only_the_older_page() {
        let blocks: Vec<Block> = (0..250)
            .map(|n| Block::User(format!("message {n}")))
            .collect();
        let (mut ws, id) = tab_with_blocks(blocks);
        let tab = ws.tab_mut(id).unwrap();
        tab.transcript_start = 50;

        let html = transcript_prepend_html(tab, 0, 50);

        assert!(html.contains("data-block=\"0\""), "{html}");
        assert!(html.contains("data-block=\"49\""), "{html}");
        assert!(!html.contains("data-block=\"50\""), "{html}");
        assert!(!html.contains("load-earlier"), "{html}");
    }

    #[test]
    fn prepend_keeps_the_control_while_more_history_remains() {
        let blocks: Vec<Block> = (0..250)
            .map(|n| Block::User(format!("message {n}")))
            .collect();
        let (mut ws, id) = tab_with_blocks(blocks);
        let tab = ws.tab_mut(id).unwrap();
        tab.transcript_start = 150;

        let html = transcript_prepend_html(tab, 50, 150);

        assert!(html.contains("data-block=\"50\""), "{html}");
        assert!(html.contains("data-block=\"149\""), "{html}");
        assert!(!html.contains("data-block=\"150\""), "{html}");
        assert!(html.contains("data-load-earlier"), "{html}");
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
    fn plugin_view_context_renders_as_a_subordinate_app_context_note() {
        let marker = "\n\n[Plugin selection context: untrusted view data]\n";
        let context = serde_json::to_string(&vec!["slide 3 selected".to_string()]).unwrap();
        let (ws, id) = tab_with_blocks(vec![Block::User(format!("make it blue{marker}{context}"))]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains(">make it blue<"), "{html}");
        assert!(html.contains("App context · 1"), "{html}");
        assert!(html.contains("It is not a plugin command"), "{html}");
        assert!(!html.contains("Plugin selection"), "{html}");
        assert!(!html.contains("untrusted view data"), "{html}");
    }

    #[test]
    fn structured_plugin_context_keeps_instance_and_view_observable() {
        let marker = "\n\n[Plugin selection context: untrusted view data]\n";
        let context = serde_json::to_string(&vec![serde_json::json!({
            "instanceId": "instance-1",
            "viewId": "view-1",
            "selection": "slide 3 selected"
        })])
        .unwrap();
        let (ws, id) = tab_with_blocks(vec![Block::User(format!("edit it{marker}{context}"))]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("instanceId"), "{html}");
        assert!(html.contains("view-1"), "{html}");
        assert!(html.contains("slide 3 selected"), "{html}");
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
        assert_eq!(tool_row("skill", "demo"), ("skill", "demo".to_string()));
        assert_eq!(tool_row("custom_tool", "{}").0, "custom_tool");
    }

    #[test]
    fn single_tool_renders_row_and_output() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a.rs\nb.rs".into()),
            images: Vec::new(),
            status: None,
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
                question: None,
                tool: "bash".into(),
                input: "first".into(),
                output: Some("one".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Assistant("between".into()),
            Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "second".into(),
                output: Some(
                    (1..=10)
                        .map(|n| format!("line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                images: Vec::new(),
                status: None,
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
             workflow_status (run_id: \"r-77\").";
        let snapshot = "run r-77 (docs): running, 1 task(s) done, 0 failed, 1 attempted\n\
             current node: polish\n\
             node read: ok, 1 done, 0 failed\n\
             node polish: pending, 0 done, 0 failed";
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                question: None,
                tool: "workflow_start".into(),
                input: spec.into(),
                output: Some(start.into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
                images: Vec::new(),
                status: None,
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
        assert!(html.contains("current <b>polish</b>"), "{html}");
        // its row title is just the run id
        assert!(html.contains(">r-77</span>"), "{html}");
    }

    fn live_run() -> gewu_protocol::WorkflowRunState {
        gewu_protocol::WorkflowRunState {
            run_id: "r-9".into(),
            name: Some("docs".into()),
            status: "running".into(),
            current_node: Some("read".into()),
            tasks_done: 1,
            tasks_failed: 0,
            tasks_attempted: 1,
            nodes: vec![
                gewu_protocol::WorkflowNodeState {
                    id: "read".into(),
                    status: "ok".into(),
                    done: 1,
                    failed: 0,
                },
                gewu_protocol::WorkflowNodeState {
                    id: "write".into(),
                    status: "pending".into(),
                    done: 0,
                    failed: 0,
                },
            ],
            error: None,
        }
    }

    #[test]
    fn workflow_start_card_draws_live_progress() {
        let spec = r#"{"name":"docs","nodes":[
            {"id":"read","agent":"reader","prompt":"p"},
            {"id":"write","agent":"writer","prompt":"p"}]}"#;
        let start = "Started workflow run r-9. It executes in the background; check on it with \
             workflow_status (run_id: \"r-9\").";
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "workflow_start".into(),
            input: spec.into(),
            output: Some(start.into()),
            images: Vec::new(),
            status: None,
        }]);
        ws.tab_mut(id).unwrap().workflows.insert(
            "r-9".into(),
            crate::workflow::WfSnapshot::from_run(&live_run()),
        );
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("start card");
        assert!(html.contains("wf-card"), "{html}");
        assert!(html.contains("wf-run-running"), "{html}");
        assert!(html.contains("wf-ok"), "{html}");
        assert!(html.contains("current <b>read</b>"), "{html}");
        assert!(html.contains("docs · 2 nodes"), "{html}");
    }

    #[test]
    fn workflow_start_without_live_state_stays_plain() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "workflow_start".into(),
            input: r#"{"name":"docs","nodes":[{"id":"read","agent":"reader","prompt":"p"}]}"#
                .into(),
            output: Some("Started workflow run r-9. It executes in the background.".into()),
            images: Vec::new(),
            status: None,
        }]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("start card");
        assert!(!html.contains("wf-card"), "{html}");
    }

    #[test]
    fn running_workflow_start_draws_a_pending_graph() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "workflow_start".into(),
            input: r#"{"name":"docs","nodes":[
                {"id":"read","agent":"reader","prompt":"p"},
                {"id":"write","agent":"writer","prompt":"p"}]}"#
                .into(),
            output: None,
            images: Vec::new(),
            status: None,
        }]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("start card");
        assert!(html.contains("wf-card"), "{html}");
        assert_eq!(html.matches("wf-pending").count(), 2, "{html}");
    }

    #[test]
    fn status_card_prefers_live_state_over_its_text_snapshot() {
        let spec = r#"{"name":"docs","nodes":[
            {"id":"read","agent":"reader","prompt":"p"},
            {"id":"write","agent":"writer","prompt":"p"}]}"#;
        let start = "Started workflow run r-9. It executes in the background.";
        let stale = "run r-9 (docs): running, 0 task(s) done, 0 failed, 0 attempted\n\
             node read: pending, 0 done, 0 failed";
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                question: None,
                tool: "workflow_start".into(),
                input: spec.into(),
                output: Some(start.into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "workflow_status".into(),
                input: "r-9".into(),
                output: Some(stale.into()),
                images: Vec::new(),
                status: None,
            },
        ]);
        ws.tab_mut(id).unwrap().workflows.insert(
            "r-9".into(),
            crate::workflow::WfSnapshot::from_run(&live_run()),
        );
        let html = tool_card_html(ws.tab(id).unwrap(), 1).expect("status card");
        assert!(html.contains("wf-ok"), "{html}");
        assert!(html.contains("current <b>read</b>"), "{html}");
        assert!(html.contains("1 done"), "{html}");
    }

    #[test]
    fn workflow_groups_collapse_on_user_toggle() {
        let snapshot = "run r-77 (docs): completed, 1 task(s) done, 0 failed, 1 attempted\n\
             node read: ok, 1 done, 0 failed";
        let start = "Started workflow run r-77. It executes in the background.";
        let (mut ws, id) = tab_with_blocks(vec![
            Block::Tool {
                question: None,
                tool: "workflow_start".into(),
                input: r#"{"name":"docs","nodes":[{"id":"read","agent":"reader","prompt":"p"}]}"#
                    .into(),
                output: Some(start.into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "workflow_status".into(),
                input: "r-77".into(),
                output: Some(snapshot.into()),
                images: Vec::new(),
                status: None,
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
            question: None,
            tool: "bash".into(),
            input: String::new(),
            output: None,
            images: Vec::new(),
            status: None,
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
            question: None,
            tool: "bash".into(),
            input: "seq 10".into(),
            output: Some(output),
            images: Vec::new(),
            status: None,
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

    fn image_part(mime: &str, data: &str) -> ContentPart {
        ContentPart::Image {
            data: data.into(),
            mime_type: mime.into(),
        }
    }

    #[test]
    fn expanded_tool_renders_image_media_inline() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "read_file".into(),
            input: "shot.png".into(),
            output: Some("Image read successfully".into()),
            images: vec![image_part("image/png", "ZmFrZQ==")],
            status: None,
        }]);

        // Collapsed: a badge marks the image, but no payload reaches the DOM.
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool-badge\">image"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("ZmFrZQ=="), "{html}");

        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(
            html.contains("<img class=\"tool-image\" src=\"data:image/png;base64,ZmFrZQ==\""),
            "{html}"
        );
        assert!(html.contains("alt=\"shot.png\""), "{html}");
        assert!(!html.contains("tool-output\">ZmFrZQ=="), "{html}");
    }

    #[test]
    fn non_raster_media_is_never_inlined() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "read_file".into(),
            input: "icon.svg".into(),
            output: Some("Image read successfully".into()),
            images: vec![image_part("image/svg+xml", "PHN2Zz48L3N2Zz4=")],
            status: None,
        }]);
        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("tool-badge"), "{html}");
        assert!(!html.contains("PHN2Zz48L3N2Zz4="), "{html}");
    }

    #[test]
    fn oversized_image_falls_back_to_a_note() {
        let data = "A".repeat(MAX_INLINE_IMAGE_BYTES + 4);
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "read_file".into(),
            input: "huge.png".into(),
            output: Some("Image read successfully".into()),
            images: vec![image_part("image/png", &data)],
            status: None,
        }]);
        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("class=\"tool-media-note\""), "{html}");
        assert!(html.contains("too large to display"), "{html}");
    }

    #[test]
    fn image_payload_is_attribute_escaped() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "read_file".into(),
            input: "shot.png".into(),
            output: Some("Image read successfully".into()),
            images: vec![image_part("image/png", "\"><script>alert(1)</script>")],
            status: None,
        }]);
        ws.tab_mut(id).unwrap().expanded_outputs.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn expanded_tool_shows_the_full_command() {
        let long_command = format!("echo {} # padded well past one line", "x".repeat(120));
        let (mut ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "bash".into(),
            input: long_command.clone(),
            output: Some("done".into()),
            images: Vec::new(),
            status: None,
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
            question: None,
            tool: "bash".into(),
            input: "seq 600".into(),
            output: Some(output),
            images: Vec::new(),
            status: None,
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
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "read_file".into(),
                input: "src/main.rs".into(),
                output: Some("b".into()),
                images: Vec::new(),
                status: None,
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
    fn skill_loads_are_not_grouped_with_neighboring_tools() {
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "read_file".into(),
                input: "src/main.rs".into(),
                output: Some("b".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "skill".into(),
                input: "demo".into(),
                output: Some("<skill_content name=\"demo\">body</skill_content>".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "pwd".into(),
                output: Some("/tmp".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "read_file".into(),
                input: "Cargo.toml".into(),
                output: Some("c".into()),
                images: Vec::new(),
                status: None,
            },
        ]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert_eq!(
            html.matches("Explored: 1 bash, 1 read").count(),
            2,
            "{html}"
        );
        assert!(html.contains("tool-name\">skill"), "{html}");
        assert!(html.contains("tool-title\">demo"), "{html}");
        assert!(!html.contains("1 skill"), "{html}");
    }

    #[test]
    fn running_tool_shows_spinner() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "bash".into(),
            input: "sleep 30".into(),
            output: None,
            images: Vec::new(),
            status: None,
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
            question: None,
            tool: "bash".into(),
            input: "ls".into(),
            output: Some("a".into()),
            images: Vec::new(),
            status: None,
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"tool depth-0\""), "{html}");
        assert!(!html.contains("tool-spinner"), "{html}");
        assert!(!html.contains("running"), "{html}");
    }

    #[test]
    fn running_tool_shows_its_live_status() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "task".into(),
            input: "explore".into(),
            output: None,
            images: Vec::new(),
            status: Some("1m03s — bash: cargo test".into()),
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(
            html.contains("<span class=\"tool-status\">1m03s — bash: cargo test</span>"),
            "{html}"
        );
        assert!(html.contains("tool-spinner"), "{html}");
    }

    #[test]
    fn tool_status_is_hidden_once_the_tool_finishes() {
        let (ws, id) = tab_with_blocks(vec![Block::Tool {
            question: None,
            tool: "task".into(),
            input: "explore".into(),
            output: Some("done".into()),
            images: Vec::new(),
            status: Some("stale".into()),
        }]);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(!html.contains("tool-status"), "{html}");
    }

    #[test]
    fn running_group_says_exploring() {
        let (ws, id) = tab_with_blocks(vec![
            Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: None,
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "grep".into(),
                input: "TODO".into(),
                output: None,
                images: Vec::new(),
                status: None,
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
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "grep".into(),
                input: "TODO".into(),
                output: Some("b".into()),
                images: Vec::new(),
                status: None,
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
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("a".into()),
                images: Vec::new(),
                status: None,
            },
            Block::Tool {
                question: None,
                tool: "grep".into(),
                input: "TODO".into(),
                output: None,
                images: Vec::new(),
                status: None,
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
        assert!(!html.contains("class=\"k\">model"), "{html}");
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
                supports_reasoning_effort: false,
                default_effort: None,
            },
            ModelSummary {
                selector: "west/small".into(),
                label: "small-model".into(),
                supports_reasoning_effort: false,
                default_effort: None,
            },
        ];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_selector = Some("west/small".into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("<select class=\"model-picker\" data-model-picker data-tab="),
            "{html}"
        );
        assert!(!html.contains("class=\"k\">model"), "{html}");
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
            supports_reasoning_effort: false,
            default_effort: None,
        }];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_label = Some("big-model".into());
        let html = status_html(ws.tab(id).unwrap(), &solo);
        assert!(!html.contains("model-picker"), "{html}");
        assert!(html.contains("class=\"v model\""), "{html}");
        assert!(!html.contains(">model</span>"), "{html}");
        assert!(html.contains("title=\"Model\""), "{html}");
    }

    #[test]
    fn status_renders_reasoning_effort_picker() {
        let models = vec![
            ModelSummary {
                selector: "west/reasoning".into(),
                label: "reasoning-model".into(),
                supports_reasoning_effort: true,
                default_effort: Some("high".into()),
            },
            ModelSummary {
                selector: "west/plain".into(),
                label: "plain-model".into(),
                supports_reasoning_effort: false,
                default_effort: None,
            },
        ];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_selector = Some("west/reasoning".into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("<select class=\"effort-picker\" data-effort-picker data-tab="),
            "{html}"
        );
        assert!(!html.contains("class=\"k\">effort"), "{html}");
        assert!(
            html.contains(
                "<option value=\"provider-default\" title=\"provider default\">default</option>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"high\" selected>high</option>"),
            "{html}"
        );
        assert!(
            !html.contains("title=\"Reasoning effort\" disabled"),
            "{html}"
        );

        // An explicit effort is selected; a busy tab disables the picker.
        let tab = ws.tab_mut(id).unwrap();
        tab.reasoning_effort = Some("max".into());
        tab.model.phase = Phase::Busy;
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("<option value=\"max\" selected>max</option>"),
            "{html}"
        );
        assert!(
            html.contains("title=\"Reasoning effort\" disabled"),
            "{html}"
        );

        // Models without effort support keep a disabled picker.
        let tab = ws.tab_mut(id).unwrap();
        tab.model.phase = Phase::Idle;
        tab.model_selector = Some("west/plain".into());
        tab.reasoning_effort = None;
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains("title=\"Reasoning effort\" disabled"),
            "{html}"
        );
        assert!(
            html.contains(
                "<option value=\"provider-default\" title=\"provider default\" selected>default</option>"
            ),
            "{html}"
        );

        // Choosing the provider default sticks as its own selection.
        let tab = ws.tab_mut(id).unwrap();
        tab.model_selector = Some("west/reasoning".into());
        tab.reasoning_effort = Some(PROVIDER_DEFAULT_EFFORT.into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains(
                "<option value=\"provider-default\" title=\"provider default\" selected>default</option>"
            ),
            "{html}"
        );
        assert!(!html.contains("<option value=\"high\" selected>"), "{html}");
    }

    #[test]
    fn effort_picker_falls_back_for_unlisted_config_values() {
        let models = [ModelSummary {
            selector: "west/custom".into(),
            label: "custom-model".into(),
            supports_reasoning_effort: true,
            default_effort: Some("vendor-defined".into()),
        }];
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model_selector = Some("west/custom".into());
        let html = status_html(ws.tab(id).unwrap(), &models);
        assert!(
            html.contains(
                "<option value=\"vendor-defined\" title=\"configured default\" selected>vendor-defined</option>"
            ),
            "{html}"
        );
        assert!(
            html.contains(
                "<option value=\"provider-default\" title=\"provider default\">default</option>"
            ),
            "{html}"
        );
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
    fn busy_status_shows_thinking_phase() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model.submit("go".into());
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("thinking…"), "{html}");
        assert!(!html.contains("Esc interrupt"), "{html}");
    }

    #[test]
    fn busy_status_shows_waiting_phase_for_a_pending_question() {
        let state = QuestionState::Pending {
            request_id: "que_1".into(),
            questions: vec![sample_question()],
        };
        let (mut ws, id) = tab_with_blocks(vec![question_block(state)]);
        ws.tab_mut(id).unwrap().model.submit("ask me".into());
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("class=\"phase waiting\""), "{html}");
        assert!(html.contains("waiting…"), "{html}");
        assert!(!html.contains("thinking…"), "{html}");
    }

    #[test]
    fn answered_or_dismissed_question_restores_the_thinking_phase() {
        for state in [
            QuestionState::Answered {
                request_id: "que_1".into(),
                questions: vec![sample_question()],
                answers: vec![vec!["SQLite".into()]],
            },
            QuestionState::Dismissed {
                request_id: "que_1".into(),
                questions: vec![sample_question()],
            },
        ] {
            let (mut ws, id) = tab_with_blocks(vec![question_block(state)]);
            ws.tab_mut(id).unwrap().model.submit("ask me".into());
            let html = status_html(ws.tab(id).unwrap(), &[]);
            assert!(html.contains("class=\"phase busy\""), "{html}");
            assert!(html.contains("thinking…"), "{html}");
            assert!(!html.contains("waiting…"), "{html}");
        }
    }

    #[test]
    fn idle_status_ignores_a_stale_pending_question() {
        let state = QuestionState::Pending {
            request_id: "que_1".into(),
            questions: vec![sample_question()],
        };
        let (ws, id) = tab_with_blocks(vec![question_block(state)]);
        let html = status_html(ws.tab(id).unwrap(), &[]);
        assert!(html.contains("idle"), "{html}");
        assert!(!html.contains("waiting…"), "{html}");
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
    fn pending_steers_render_a_queue_badge() {
        let (mut ws, id) = tab_with_blocks(vec![Block::User("hi".into())]);
        {
            let tab = ws.tab_mut(id).unwrap();
            tab.model.apply_event(&AgentEvent::Steered {
                text: "focus".into(),
            });
            tab.pending_steers.push(("focus".into(), 1));
        }
        let html = block_html(ws.tab(id).unwrap(), 1).expect("steered block renders");
        assert!(
            html.contains("<span class=\"queue-status\">queued</span>"),
            "{html}"
        );
        // The settled first message carries no badge.
        let html = block_html(ws.tab(id).unwrap(), 0).unwrap();
        assert!(!html.contains("queue-status"), "{html}");

        // Confirmation (the pending entry is consumed) settles the badge.
        ws.tab_mut(id).unwrap().pending_steers.clear();
        let html = block_html(ws.tab(id).unwrap(), 1).unwrap();
        assert!(!html.contains("queue-status"), "{html}");
    }

    #[test]
    fn queue_badge_requires_matching_text_and_index() {
        let (mut ws, id) = tab_with_blocks(vec![Block::User("hi".into())]);
        // A stale pending index (or different text) must not badge the block.
        {
            let tab = ws.tab_mut(id).unwrap();
            tab.pending_steers.push(("other".into(), 0));
        }
        let html = block_html(ws.tab(id).unwrap(), 0).unwrap();
        assert!(!html.contains("queue-status"), "{html}");
    }

    fn question_block(question: QuestionState) -> Block {
        Block::Tool {
            question: Some(question),
            tool: "question".into(),
            input: "Database".into(),
            output: None,
            images: Vec::new(),
            status: None,
        }
    }

    fn sample_question() -> gewu_protocol::Question {
        gewu_protocol::Question {
            question: "Which database?".into(),
            header: "Database".into(),
            options: vec![
                gewu_protocol::QuestionOption {
                    label: "Postgres".into(),
                    description: "Relational".into(),
                },
                gewu_protocol::QuestionOption {
                    label: "SQLite".into(),
                    description: String::new(),
                },
            ],
            multiple: false,
            custom: true,
        }
    }

    #[test]
    fn pending_question_card_renders_tabbed_form() {
        let multi = gewu_protocol::Question {
            question: "Which features?".into(),
            header: "Features".into(),
            options: vec![gewu_protocol::QuestionOption {
                label: "Auth".into(),
                description: "Login".into(),
            }],
            multiple: true,
            custom: false,
        };
        let state = QuestionState::Pending {
            request_id: "que_1".into(),
            questions: vec![sample_question(), multi],
        };
        let (ws, id) = tab_with_blocks(vec![question_block(state)]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("question card");

        // The wrapper carries the card's distinct accent class.
        assert!(html.contains("class=\"tool-card question-card\""), "{html}");
        assert!(html.contains("class=\"question-form\""), "{html}");
        assert!(html.contains("data-qtab=\"0\""), "{html}");
        assert!(html.contains("data-qtab=\"1\""), "{html}");
        assert!(html.contains("type=\"radio\""), "{html}");
        assert!(html.contains("type=\"checkbox\""), "{html}");
        assert!(html.contains("name=\"q-que_1-0\""), "{html}");
        assert!(html.contains("data-custom=\"0\""), "{html}");
        assert!(html.contains("question-submit"), "{html}");
        assert!(html.contains("data-question-dismiss"), "{html}");
        // Forms are not click-to-toggle tool cards.
        assert!(!html.contains("toggle_tool"), "{html}");
        // The custom row is absent where the question forbids it.
        assert!(!html.contains("data-custom=\"1\""), "{html}");
    }

    #[test]
    fn question_card_escapes_model_authored_text() {
        let state = QuestionState::Pending {
            request_id: "que_1".into(),
            questions: vec![gewu_protocol::Question {
                question: "<script>alert(1)</script>".into(),
                header: "\"><img src=x>".into(),
                options: vec![gewu_protocol::QuestionOption {
                    label: "\"><script>x</script>".into(),
                    description: "<b>bad</b>".into(),
                }],
                multiple: false,
                custom: true,
            }],
        };
        let (ws, id) = tab_with_blocks(vec![question_block(state)]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("question card");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<img src=x>"), "{html}");
        assert!(!html.contains("<b>bad</b>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        // A single question has no tab strip.
        assert!(!html.contains("question-tabs"), "{html}");
    }

    #[test]
    fn settled_question_card_shows_answers_or_dismissal() {
        let questions = vec![sample_question()];
        let answered = QuestionState::Answered {
            request_id: "que_1".into(),
            questions: questions.clone(),
            answers: vec![vec!["SQLite".into()]],
        };
        let (ws, id) = tab_with_blocks(vec![question_block(answered)]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("question card");
        assert!(html.contains("question-answer"), "{html}");
        assert!(html.contains("SQLite"), "{html}");
        assert!(!html.contains("question-form"), "{html}");

        let dismissed = QuestionState::Dismissed {
            request_id: "que_2".into(),
            questions,
        };
        let (ws, id) = tab_with_blocks(vec![question_block(dismissed)]);
        let html = tool_card_html(ws.tab(id).unwrap(), 0).expect("question card");
        assert!(html.contains("Dismissed"), "{html}");
    }

    #[test]
    fn question_cards_are_standalone_runs() {
        let question = question_block(QuestionState::Pending {
            request_id: "que_1".into(),
            questions: vec![sample_question()],
        });
        let bash = Block::Tool {
            question: None,
            tool: "bash".into(),
            input: "ls".into(),
            output: None,
            images: Vec::new(),
            status: None,
        };
        assert!(!same_tool_run(&question, &bash));
        assert!(!same_tool_run(&bash, &question));
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
