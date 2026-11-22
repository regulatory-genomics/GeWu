use crate::endpoint::Endpoint;
use anyhow::{Result, bail};
use gewu_client::{AskStream, Client};
use gewu_protocol::{AgentEvent, ServerMessage};
use gewu_server::bootstrap::CommonArgs;
use serde_json::{Value, json};
use std::io::{self, IsTerminal, Read, Write};

#[derive(clap::Args)]
pub struct ExecArgs {
    /// Connect to this WebSocket server (default: launch a private local server)
    #[arg(long)]
    pub server: Option<String>,
    /// Emit one JSON object per line
    #[arg(long)]
    pub json: bool,
    /// Initial instructions; piped text is appended
    pub prompt: Option<String>,
}

fn prompt(arg: Option<String>) -> Result<String> {
    let mut piped = String::new();
    if !io::stdin().is_terminal() {
        io::stdin().read_to_string(&mut piped)?;
    }
    match (
        arg.filter(|s| !s.trim().is_empty() && s != "-"),
        piped.trim().is_empty(),
    ) {
        (Some(text), false) => Ok(format!("{text}\n\n<stdin>\n{piped}\n</stdin>")),
        (Some(text), true) => Ok(text),
        (None, false) => Ok(piped),
        _ => bail!("provide a prompt or pipe text to gewu exec"),
    }
}

struct Output {
    json: bool,
    session: Option<String>,
}

impl Output {
    fn record(&self, kind: &str, mut fields: Value) -> Result<()> {
        fields["type"] = kind.into();
        fields["session_id"] = json!(self.session);
        let mut out = io::stdout().lock();
        writeln!(out, "{fields}")?;
        out.flush()?;
        Ok(())
    }

    fn failure(&self, message: &str, code: u8) {
        if self.json {
            let _ = self.record("error", json!({"error": message, "exit_code": code}));
        }
        eprintln!("{message}");
    }

    async fn turn(&mut self, client: &Client, prompt: String, agent: &str) -> Result<u8> {
        let mut ask = client.ask(prompt);
        if !agent.is_empty() {
            ask = ask.agent(agent);
        }
        let mut stream = ask.send().await?;
        self.session = client.session_id().await;
        if self.json {
            self.record("started", json!({}))?;
        }
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        loop {
            let frame = tokio::select! {
                frame = stream.next_frame() => frame,
                result = &mut interrupt => {
                    result?;
                    client.interrupt().await?;
                    self.failure("interrupted", 130);
                    return Ok(130);
                }
            };
            match frame.transpose()? {
                Some(ServerMessage::Result { response, .. }) => {
                    if self.json {
                        self.record("result", json!({"response": response}))?;
                    } else {
                        writeln!(io::stdout().lock(), "{response}")?;
                    }
                    return Ok(0);
                }
                Some(ServerMessage::Error { error, .. }) => bail!("{error}"),
                Some(frame) => {
                    if let Some(event) = AskStream::decode_event(&frame) {
                        let event = event?;
                        if self.json {
                            self.record("event", json!({"event": event}))?;
                        }
                        match event {
                            AgentEvent::QuestionAsked { questions, .. } => {
                                client.interrupt().await?;
                                let labels: Vec<_> =
                                    questions.iter().map(|q| q.question.as_str()).collect();
                                let id = self.session.as_deref().unwrap_or("unknown");
                                self.failure(
                                    &format!("session {id} requires input: {}", labels.join("\n")),
                                    3,
                                );
                                return Ok(3);
                            }
                            AgentEvent::ToolStart { tool, input } if !self.json => {
                                eprintln!(
                                    "{tool}: {}",
                                    gewu_client::summarize_tool_input(&tool, &input)
                                );
                            }
                            AgentEvent::Status { message } if !self.json => eprintln!("{message}"),
                            _ => {}
                        }
                    }
                }
                None => bail!("connection closed before the task completed"),
            }
        }
    }
}

pub async fn exec(args: ExecArgs, common: &CommonArgs, token: Option<String>) -> u8 {
    let mut output = Output {
        json: args.json,
        session: None,
    };
    let result = async {
        let prompt = prompt(args.prompt)?;
        gewu_server::bootstrap::init_tracing_with_writer(&common.log_level, std::io::stderr)?;
        let endpoint = Endpoint::open(args.server, token, common).await?;
        let run = async {
            let client = endpoint.connect().await?;
            let result = output.turn(&client, prompt, &common.agent).await;
            client.close().await;
            result
        }
        .await;
        let shutdown = endpoint.close().await;
        let code = run?;
        shutdown?;
        Ok::<_, anyhow::Error>(code)
    }
    .await;
    result.unwrap_or_else(|error| {
        output.failure(&format!("{error:#}"), 1);
        1
    })
}
