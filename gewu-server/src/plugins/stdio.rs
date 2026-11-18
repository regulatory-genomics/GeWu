//! One supervised, full-duplex adapter connection per generation.
use anyhow::{Context, Result, anyhow, bail};
use gewu_protocol::plugin::*;
use gewu_tools::process::{ProcessScope, read_frame};
use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
};

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    #[serde(rename = "startupMs", deserialize_with = "millis")]
    pub startup: Duration,
    #[serde(rename = "callMs", deserialize_with = "millis")]
    pub call: Duration,
    #[serde(rename = "shutdownMs", deserialize_with = "millis")]
    pub shutdown: Duration,
    #[serde(rename = "cleanupMs", deserialize_with = "millis")]
    pub cleanup: Duration,
    pub stderr_bytes: usize,
}

fn millis<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Duration, D::Error> {
    let ms: u64 = serde::Deserialize::deserialize(d)?;
    if !(1..=86_400_000).contains(&ms) {
        return Err(serde::de::Error::custom("expected 1–86400000 ms"));
    }
    Ok(Duration::from_millis(ms))
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(60),
            call: Duration::from_secs(1800),
            shutdown: Duration::from_secs(5),
            cleanup: Duration::from_secs(5),
            stderr_bytes: 64 * 1024,
        }
    }
}

type Reply = Result<Response, Failure>;
struct Work {
    request: Request,
    progress: Option<mpsc::Sender<Event>>,
    reply: oneshot::Sender<Reply>,
}

#[derive(Clone)]
pub struct Adapter {
    requests: mpsc::Sender<Work>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<Option<bool>>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    limits: Limits,
    binding: Arc<Activation>,
}

fn failure(code: &str, message: &str) -> Failure {
    Failure {
        code: code.into(),
        message: message.into(),
        retryable: false,
    }
}

impl Adapter {
    pub async fn start(
        mut command: Command,
        activation: Activation,
        limits: Limits,
    ) -> Result<(Self, Activated)> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut scope =
            ProcessScope::spawn(command).context("could not start plugin executable")?;
        let mut stdin = scope.child_mut().stdin.take().unwrap();
        let mut stdout = BufReader::new(scope.child_mut().stdout.take().unwrap());
        let mut stderr = scope.child_mut().stderr.take().unwrap();
        let (requests, rx) = mpsc::channel(16);
        let (stop, stop_rx) = watch::channel(false);
        let (finished, done) = watch::channel(None);
        let (frames, frame_rx) = mpsc::channel(8);
        let (writes, mut write_rx) = mpsc::channel::<HostFrame>(8);
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let adapter = Self {
            requests,
            stop,
            done,
            stderr: log.clone(),
            limits: limits.clone(),
            binding: Arc::new(activation.clone()),
        };
        let reader = tokio::spawn(async move {
            while let Ok(Some(line)) = read_frame(&mut stdout, MAX_FRAME_BYTES).await {
                match decode::<AdapterFrame>(&line) {
                    Ok(frame) => {
                        if frames.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let writer = tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                let mut data = serde_json::to_vec(&frame)?;
                anyhow::ensure!(
                    data.len() <= MAX_FRAME_BYTES,
                    "outgoing frame exceeds 1 MiB"
                );
                data.push(b'\n');
                stdin.write_all(&data).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let stderr_cap = limits.stderr_bytes;
        let logs = tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                let mut tail = log.lock().unwrap();
                tail.extend(&chunk[..n]);
                let excess = tail.len().saturating_sub(stderr_cap);
                tail.drain(..excess);
            }
        });
        tokio::spawn(async move {
            let cooperative = dispatch(&scope, rx, frame_rx, writes, stop_rx).await;
            reader.abort();
            writer.abort();
            let clean = (cooperative && scope.wait(limits.shutdown).await.is_ok())
                || scope.stop(limits.cleanup).await.is_ok();
            logs.abort();
            let _ = finished.send(Some(clean));
        });
        match adapter.request(Operation::Activate(activation), None).await {
            Ok(Response::Activated(catalog)) => Ok((adapter, catalog)),
            result => {
                adapter.close().await?;
                match result {
                    Err(e) => Err(anyhow!("{}: {}", e.code, e.message)),
                    Ok(_) => bail!("adapter did not return a catalog"),
                }
            }
        }
    }

    pub async fn request(
        &self,
        operation: Operation,
        progress: Option<mpsc::Sender<Event>>,
    ) -> Reply {
        if let Operation::Call { context, .. } = &operation
            && (context.generation != self.binding.generation
                || context.instance_id != self.binding.instance_id
                || context.workspace_id != self.binding.workspace_id)
        {
            return Err(failure(
                "STALE_BINDING",
                "call belongs to a different adapter generation or workspace",
            ));
        }
        let timeout = match operation {
            Operation::Activate(_) => self.limits.startup,
            Operation::Call { .. } => self.limits.call,
            Operation::Resolve(_) => self.limits.call.min(Duration::from_secs(30)),
            _ => self.limits.shutdown,
        };
        let (reply, receive) = oneshot::channel();
        let request = Request {
            id: uuid::Uuid::new_v4().to_string(),
            operation,
        };
        if serde_json::to_vec(&request).map_or(true, |bytes| bytes.len() > MAX_FRAME_BYTES) {
            return Err(failure("INVALID_REQUEST", "outgoing frame exceeds 1 MiB"));
        }
        match tokio::time::timeout(timeout, async {
            self.requests
                .send(Work {
                    request,
                    progress,
                    reply,
                })
                .await
                .ok()?;
            receive.await.ok()
        })
        .await
        {
            Ok(Some(reply)) => reply,
            Ok(None) => Err(failure(
                "OUTCOME_UNKNOWN",
                "adapter disconnected before settling the request",
            )),
            Err(_) => {
                // A stalled cancel must not take down unrelated work in the instance.
                Err(failure(
                    "OUTCOME_UNKNOWN",
                    "adapter did not settle before the deadline",
                ))
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }

    pub fn stderr_tail(&self) -> Vec<u8> {
        self.stderr.lock().unwrap().iter().copied().collect()
    }

    pub async fn shutdown(&self, reason: &str) -> Result<()> {
        let result = self
            .request(
                Operation::Deactivate {
                    reason: reason.into(),
                },
                None,
            )
            .await;
        if let Err(error) = result {
            self.close().await?;
            bail!(
                "{}: {} (owned process scope cleaned)",
                error.code,
                error.message
            );
        }
        self.await_cleanup().await
    }

    pub async fn close(&self) -> Result<()> {
        let _ = self.stop.send(true);
        self.await_cleanup().await
    }

    async fn await_cleanup(&self) -> Result<()> {
        let mut done = self.done.clone();
        if let Ok(Ok(value)) = tokio::time::timeout(
            self.limits.shutdown + self.limits.cleanup + Duration::from_secs(1),
            done.wait_for(Option::is_some),
        )
        .await
            && *value == Some(true)
        {
            return Ok(());
        }
        bail!("CLEANUP_INCOMPLETE: could not confirm owned process exit")
    }
}

async fn dispatch(
    scope: &ProcessScope,
    mut requests: mpsc::Receiver<Work>,
    mut frames: mpsc::Receiver<AdapterFrame>,
    writes: mpsc::Sender<HostFrame>,
    mut stop: watch::Receiver<bool>,
) -> bool {
    let mut pending = Pending::default();
    let mut calls: HashMap<String, Work> = HashMap::new();
    let mut groups = HashMap::new();
    let mut stopping = false;
    let cooperative = loop {
        tokio::select! {
            _ = stop.changed() => break false,
            work = requests.recv() => {
                let Some(work) = work else { break false; };
                if stopping || calls.len() >= 256 {
                    let _ = work.reply.send(Err(failure("ADAPTER_BUSY", "adapter is stopping or has too many pending requests")));
                    continue;
                }
                stopping = matches!(work.request.operation, Operation::Deactivate {..});
                if pending.start(&work.request).is_err() || writes.try_send(HostFrame::Request(work.request.clone())).is_err() { break false; }
                calls.insert(work.request.id.clone(), work);
            }
            frame = frames.recv() => {
                let Some(frame) = frame else { break false; };
                if pending.receive(&frame).is_err() { break false; }
                match frame {
                    AdapterFrame::Event {id, event: Event::ProcessStarted {pid, pgid}} => {
                        if groups.contains_key(&pid) || scope.register(pid, pgid).is_err() || writes.try_send(HostFrame::Control(Control::ProcessAccepted {id, pid, pgid})).is_err() { break false; }
                        groups.insert(pid, pgid);
                    }
                    AdapterFrame::Event {event: Event::ProcessExited {pid, pgid}, ..} => {
                        if groups.remove(&pid) != Some(pgid) { break false; }
                    }
                    AdapterFrame::Event {id, event} => if let Some(progress) = &calls[&id].progress { let _ = progress.try_send(event); },
                    AdapterFrame::Result {id, result} => {
                        let Some(work) = calls.remove(&id) else { break false; };
                        let shutdown = matches!(result, Response::Deactivated {..});
                        if shutdown && (!calls.is_empty() || !groups.is_empty()) { break false; }
                        let _ = work.reply.send(Ok(result));
                        if shutdown { break true; }
                    }
                    AdapterFrame::Error {id, error} => {
                        let Some(work) = calls.remove(&id) else { break false; };
                        let _ = work.reply.send(Err(error));
                    }
                }
            }
        }
    };
    cooperative
}
