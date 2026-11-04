//! HTML rendering for the web GUI.
//!
//! Mirrors `gewu-tui`'s `tui_view`: transcript blocks, Markdown, tool runs,
//! and the status line. Everything user- or model-authored is escaped, and
//! raw HTML in Markdown is neutralized.

use std::fmt::Write as _;

use gewu_client::session::{Block, Phase};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

use crate::workspace::Tab;

/// Lines of tool output shown before collapsing.
const TOOL_PREVIEW_LINES: usize = 6;
/// Defensive cap on expanded tool output.
const TOOL_MAX_LINES: usize = 500;

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
            render_block(&mut out, &tab.model.blocks[index], index, streaming);
            index += 1;
        }
    }
    out
}

/// Render one non-tool transcript block.
fn render_block(out: &mut String, block: &Block, index: usize, streaming: bool) {
    let class = match block {
        Block::User(_) => "user",
        Block::Assistant(_) => "assistant",
        Block::Error(_) => "error",
        Block::Info(_) => "info",
        Block::Tool { .. } => return,
    };
    let _ = writeln!(out, "<div class=\"block {class}\" data-block=\"{index}\">");
    match block {
        Block::Assistant(text) if !streaming => {
            let _ = writeln!(
                out,
                "<div class=\"body markdown\">{}</div>",
                markdown_to_html(text)
            );
        }
        Block::Assistant(text) => {
            let _ = writeln!(
                out,
                "<div class=\"body streaming\"><span class=\"streaming-text\" data-block=\"{index}\">{}</span></div>",
                escape(text)
            );
        }
        _ => {
            let _ = writeln!(
                out,
                "<div class=\"body plain\">{}</div>",
                escape(block.text())
            );
        }
    }
    out.push_str("</div>\n");
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

    out.push_str("<div class=\"tool-card\">\n");
    if tools.len() == 1 {
        let (tool, input, output) = tools[0];
        render_tool(out, tab, tool, input, output, first, 0);
    } else {
        let expanded = tab.expanded_groups.contains(&first);
        let glyph = if expanded { "−" } else { "+" };
        let _ = writeln!(
            out,
            "<div class=\"tool-group\" data-block=\"{first}\">\
             <button class=\"tool-summary\" data-action=\"toggle_group\" data-tab=\"{}\" data-block=\"{first}\">{glyph} {}</button>",
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
    let (name, mut title) = tool_row(tool, input);
    if output.is_none() {
        title.push_str(" …");
    }
    let expanded = tab.expanded_outputs.contains(&index);
    let _ = writeln!(
        out,
        "<div class=\"tool depth-{depth}\" data-block=\"{index}\">\
         <button class=\"tool-row\" data-action=\"toggle_tool\" data-tab=\"{}\" data-block=\"{index}\">\
         <span class=\"tool-name\">{}</span> <span class=\"tool-title\">{}</span></button>",
        tab.id,
        escape(name),
        escape(&title)
    );

    let Some(output) = output else {
        out.push_str("</div>\n");
        return;
    };
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
        other => other,
    }
}

fn tool_row<'a>(tool: &'a str, input: &str) -> (&'a str, String) {
    let title = if input.is_empty() {
        tool.to_string()
    } else {
        input.to_string()
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

/// Markdown to safe HTML.
///
/// Raw HTML is converted to escaped text, and dangerous link/image schemes
/// are replaced with `#`, so model-authored content cannot inject script.
pub fn markdown_to_html(text: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(text, options).map(|event| match event {
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
/// and a context gauge on the right.
pub fn status_html(tab: &Tab) -> String {
    let mut out = String::from("<div class=\"status-inner\">");
    match tab.model.phase {
        Phase::Idle => out.push_str("<span class=\"phase\"><i class=\"dot\"></i>idle</span>"),
        Phase::Busy => {
            out.push_str("<span class=\"phase busy\"><i class=\"dot\"></i>thinking…</span>");
        }
    }
    if let Some(label) = &tab.model_label {
        let _ = write!(
            out,
            "<span class=\"kv\"><span class=\"k\">model</span><span class=\"v model\">{}</span></span>",
            escape(label)
        );
    }
    let keys = if tab.model.phase == Phase::Busy {
        "Esc interrupt"
    } else {
        "Enter send · Shift+Enter newline"
    };
    let _ = write!(out, "<span class=\"hint\">{keys}</span>");

    if let Some((used, limit)) = tab.context.filter(|(used, limit)| *used > 0 && *limit > 0) {
        let pct = (used * 100 / limit).min(100);
        let class = if pct >= 80 { "usage warn" } else { "usage" };
        // The CSP forbids inline styles, so the gauge width snaps to 5% steps.
        let step = pct / 5 * 5;
        let _ = write!(
            out,
            "<span class=\"{class}\" title=\"{used} / {limit} tokens\">\
             <span class=\"k\">ctx</span>\
             <span class=\"gauge\" aria-hidden=\"true\"><span class=\"gauge-fill g{step}\"></span></span>\
             <span class=\"v\">{} / {} ({}%)</span></span>",
            format_k(used),
            format_k(limit),
            pct
        );
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

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
    fn streaming_assistant_is_raw_and_escaped() {
        let (mut ws, id) = tab_with_blocks(vec![Block::Assistant("<b>partial".into())]);
        ws.tab_mut(id).unwrap().model.phase = Phase::Busy;
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"body streaming\""), "{html}");
        assert!(!html.contains("class=\"label\""), "{html}");
        assert!(!html.contains(">gewu<"), "{html}");
        assert!(html.contains("&lt;b&gt;partial"), "{html}");
        assert!(!html.contains("<strong>"), "streaming is not Markdown");
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
        assert!(html.contains("class=\"tool-card\""), "{html}");
        assert!(html.contains("tool-name\">bash"), "{html}");
        assert!(html.contains("tool-title\">ls"), "{html}");
        assert!(html.contains("a.rs\nb.rs"), "{html}");
        assert!(html.contains("data-action=\"toggle_tool\""), "{html}");
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

        ws.tab_mut(id).unwrap().expanded_groups.insert(0);
        let html = transcript_html(ws.tab(id).unwrap());
        assert!(html.contains("− Explored: 1 bash, 1 read"), "{html}");
        assert!(html.contains("src/main.rs"), "{html}");
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
    }

    #[test]
    fn status_shows_model_and_usage() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        let tab = ws.tab_mut(id).unwrap();
        tab.model_label = Some("openai/gpt-4o".into());
        tab.context = Some((34_000, 128_000));
        let html = status_html(ws.tab(id).unwrap());
        assert!(html.contains("openai/gpt-4o"), "{html}");
        assert!(!html.contains("session"), "{html}");
        assert!(html.contains("34k / 128k (26%)"), "{html}");
        assert!(html.contains("gauge-fill g25"), "{html}");
        assert!(html.contains("class=\"usage\""), "{html}");
    }

    #[test]
    fn status_usage_warns_near_limit() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().context = Some((110_000, 128_000));
        let html = status_html(ws.tab(id).unwrap());
        assert!(html.contains("class=\"usage warn\""), "{html}");
        assert!(html.contains("(85%)"), "{html}");
        assert!(html.contains("gauge-fill g85"), "{html}");
    }

    #[test]
    fn busy_status_shows_interrupt_hint() {
        let (mut ws, id) = tab_with_blocks(vec![]);
        ws.tab_mut(id).unwrap().model.submit("go".into());
        let html = status_html(ws.tab(id).unwrap());
        assert!(html.contains("thinking…"), "{html}");
        assert!(html.contains("Esc interrupt"), "{html}");
    }
}
