# gewu-tui

Full-screen terminal UI for a GeWu server (`gewu-server serve`), built on the
[`gewu-client`](../gewu-client/README.md) library.

Start a server first:

```bash
cargo run -p gewu-server -- serve
```

Then launch the TUI:

```bash
cargo run -p gewu-tui
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--host` | `127.0.0.1` | Server host |
| `--tcp-port` | `8421` | Server TCP port (newline-delimited JSON) |
| `--session-id` | — | Focus a specific stored session's tab (also via `GEWU_SESSION_ID`) |
| `--new` | off | Focus a fresh session tab instead of the most recent conversation |

## Tabs

On startup the TUI opens **every stored session as a tab** — oldest on the
left, newest on the right — and focuses the newest. Tab titles come from the
conversation's first prompt (falling back to the short session id, or `new`
for a tab that never sent anything). Switch tabs by clicking one, with
`Ctrl+PageUp`/`Ctrl+PageDown`, or `Alt+1`…`Alt+9`. The `+` button sits right
after the last tab (or `Ctrl+T`) and opens a fresh session in a new tab at the
right end; each tab talks to the server over its own connection, so a reply
can keep streaming in a background tab while you work in another.

The bar itself is deliberately quiet: thin dim `│` rules separate the tabs,
the focused tab's label is underlined and accented in cyan (no inverted
block), and a tab whose reply is still streaming shows a cyan `…` so you can
spot background activity at a glance.

The `×` on a tab **closes it and permanently deletes that session from the
server's store** — there is no confirmation and no undo. `Ctrl+W` does the
same for the focused tab. If deletion fails (e.g. a server too old for the
`delete_session` op), the tab stays open with an error note instead of
vanishing silently.

On exit, the client prints each open tab's session id to stderr
(`[session: <id>]`) so you can capture one and resume it later:

```bash
GEWU_SESSION_ID=<id> cargo run -p gewu-tui   # focus a specific conversation
```

## Keybindings

| Key | Action |
|-----|--------|
| `Enter` | Send the prompt (streams the reply into the transcript) |
| `Esc` | Interrupt the running ask |
| `↑`/`↓` | Scroll the transcript (when the input is empty; works while streaming) |
| `PageUp`/`PageDown` | Scroll faster |
| `Home`/`End` | Jump to transcript top / bottom (when the input is empty), else move the input cursor |
| `Backspace`/`Delete`, `←`/`→` | Edit the input |
| `Ctrl+C`/`Ctrl+D` | Quit (Ctrl+D only when idle with an empty input) |
| `Ctrl+T` | Open a fresh session tab (same as the `+` button) |
| `Ctrl+W` | Close the current tab and delete its session from the store |
| `Ctrl+PageUp`/`Ctrl+PageDown` | Previous / next tab |
| `Alt+1`…`Alt+9` | Jump to the Nth tab |

Touchpad and mouse-wheel scrolling is captured by the TUI and scrolls the
transcript, including over SSH (where the terminal's own scrollback is not
usable in the alternate screen). Because mouse capture is on, dragging to
select text requires holding **Shift**, as in vim or tmux.

While a reply streams, the status line shows `thinking…`. Tool calls render
like OpenCode's TUI: an inline row per call, with no `tool` label and no box.
The row names the tool and shows its main argument — `bash  git status`,
`read  src/main.rs`, `write  README.md`, `grep  TODO`, `fetch  https://…`, or
`custom_tool  {"a":1}` for anything else. The tool name is dim; the command or
path is default foreground so the actionable part is easy to read. A tool still
running ends in `…`. Output appears beneath the row, collapsed to the first few
lines with `… +N lines` when it overflows; a tool's row plus output can be
**clicked** to show or hide the rest. Runs of two or more consecutive tool calls
collapse into a `+ Explored: 2 bash, 1 read` summary that expands on click
(`Explored` once every call finished, `Exploring` while one still runs). The
status line also shows the server's model as `provider/model` (e.g.
`openai/gpt-4o`), whenever the server reports it. At the bottom right, the
status line shows the **context usage** as `40 (0%)` — estimated conversation
tokens and the percentage of the model's context window (the same approximate
estimate the agent's compaction uses). It turns yellow at 80% to warn that
compaction is near; the key hints yield to it on narrow terminals. Scrolling up
pins the history so streaming output doesn't yank you back to the bottom (the
status line shows `↑N more`); scroll back to the bottom, or press `End`, to
re-follow.

## Colors

Important text — user prompts, assistant replies, and errors — uses the
terminal's default foreground (white on most themes). Markdown headings are
the exception: they use the accent cyan. Supporting details are dimmed: tool
names and their output, informational status notes, and the status line. Tool
commands and paths stay at the default foreground.

## Markdown

Finished **assistant replies** are rendered as Markdown: headings, bold and
italic text, inline code, fenced code blocks with syntax highlighting, lists,
block quotes, links, and Unicode-bordered tables. Long lines and tables are
wrapped to the width of the transcript pane.

Heading markers (`#`/`##`) are concealed. Headings render in accent cyan and
bold — H1 is also underlined — mirroring OpenCode's TUI, since terminals have
no font sizes to convey hierarchy.

The reply currently streaming is shown as raw text until it finishes, so
half-typed syntax (such as an unclosed code fence) doesn't flicker. User
prompts, errors, and status notes always stay plain.

Rendered blocks are cached per transcript entry and re-rendered only when their
text changes or the window is resized, so long conversations stay cheap to
redraw.

## Layout

```
gewu-tui
  main.rs      CLI flags + terminal event loop
  resume.rs    startup history resume (pick newest session, replay it)
  tui.rs       App state (composer, scroll) over gewu_client::session::SessionModel
  tui_view.rs  ratatui rendering
```

Conversation state lives in `gewu-client::session` and is shared with the web
GUI ([`gewu-gui`](../gewu-gui/README.md)), so both front-ends render events
and replay history identically.
