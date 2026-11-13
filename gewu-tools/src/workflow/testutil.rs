//! Test doubles shared by the workflow module's tests.
//!
//! [`ScriptedFactory`] spawns [`gewu_core::Agent`]s on a scripted
//! [`LlmProvider`]: each requested agent name pops the next response from
//! its queue (the last repeats), and can be made to fail at creation, at
//! stream time, or mid-stream.

use crate::task::{AgentFactory, AgentOverrides};
use futures_util::stream::BoxStream;
use gewu_core::Agent;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The owner session id the tests start runs under.
pub(crate) const OWNER: &str = "owner-session";

/// Response prefix that makes the scripted LLM emit a provider error
/// instead of content.
pub(crate) const ERROR_MARKER: &str = "@@error@@ ";

/// One recorded agent-spawn request.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub base: String,
    pub instructions: Option<String>,
    pub tools: Option<Vec<String>>,
    pub parent_session: String,
}

struct ScriptedLlm {
    response: String,
    fail_stream: bool,
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedLlm {
    fn build(
        &self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        Ok(serde_json::json!({ "model": "mock" }))
    }

    async fn send<'a>(
        &'a self,
        _request: serde_json::Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        if self.fail_stream {
            return Err("stream failed".into());
        }
        if let Some(error) = self.response.strip_prefix(ERROR_MARKER) {
            return Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                ProviderEvent::Error {
                    error: error.to_string(),
                },
            )])));
        }
        Ok(Box::pin(futures_util::stream::iter(vec![
            Ok(ProviderEvent::ContentDelta {
                delta: self.response.clone(),
            }),
            Ok(ProviderEvent::Finish { reason: None }),
        ])))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
}

/// An [`AgentFactory`] whose agents answer with scripted responses.
pub(crate) struct ScriptedFactory {
    delay: Mutex<Duration>,
    /// Responses per agent name; each call pops the next entry, the last
    /// one repeats.
    responses: Mutex<HashMap<String, VecDeque<String>>>,
    fail_create: Mutex<HashSet<String>>,
    fail_stream: Mutex<HashSet<String>>,
    sessions: AtomicUsize,
    pub(crate) calls: Mutex<Vec<Call>>,
    /// Currently executing agent calls (for concurrency assertions).
    pub(crate) running: Arc<AtomicUsize>,
    /// High-water mark of `running`.
    pub(crate) peak: Arc<AtomicUsize>,
}

impl ScriptedFactory {
    pub(crate) fn new(delay: Duration) -> Self {
        Self {
            delay: Mutex::new(delay),
            responses: Mutex::new(HashMap::new()),
            fail_create: Mutex::new(HashSet::new()),
            fail_stream: Mutex::new(HashSet::new()),
            sessions: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            running: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// Queues responses for `name`; the last one repeats once the queue
    /// runs dry (the default response is `"ok"`).
    pub(crate) fn script(&self, name: &str, responses: &[&str]) {
        self.responses.lock().unwrap().insert(
            name.to_string(),
            responses.iter().map(|text| text.to_string()).collect(),
        );
    }

    pub(crate) fn fail_create(&self, name: &str) {
        self.fail_create.lock().unwrap().insert(name.to_string());
    }

    pub(crate) fn fail_stream(&self, name: &str) {
        self.fail_stream.lock().unwrap().insert(name.to_string());
    }

    async fn spawn(
        &self,
        base: &str,
        _overrides: &AgentOverrides,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        if self.fail_create.lock().unwrap().contains(base) {
            return Err(format!("no agent named '{base}'").into());
        }
        let slot = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(slot, Ordering::SeqCst);
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        self.running.fetch_sub(1, Ordering::SeqCst);

        let responses = self.responses.lock().unwrap();
        let response = responses
            .get(base)
            .and_then(|queue| queue.front().cloned())
            .unwrap_or_else(|| "ok".to_string());
        drop(responses);
        if let Some(queue) = self.responses.lock().unwrap().get_mut(base)
            && queue.len() > 1
        {
            queue.pop_front();
        }

        let fail_stream = self.fail_stream.lock().unwrap().contains(base);
        let agent = Agent::builder()
            .llm(Arc::new(ScriptedLlm {
                response,
                fail_stream,
            }))
            .build()
            .map_err(|e| -> Box<dyn Error + Send + Sync> { e.to_string().into() })?;
        let session = format!("s-{}", self.sessions.fetch_add(1, Ordering::SeqCst));
        let _ = parent_session;
        Ok((agent, session))
    }
}

#[async_trait::async_trait]
impl AgentFactory for ScriptedFactory {
    async fn create(
        &self,
        subagent_type: &str,
        resume: Option<String>,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        self.calls.lock().unwrap().push(Call {
            base: subagent_type.to_string(),
            instructions: None,
            tools: None,
            parent_session: parent_session.to_string(),
        });
        let _ = &resume;
        self.spawn(subagent_type, &AgentOverrides::default(), parent_session)
            .await
    }

    async fn create_with(
        &self,
        base: &str,
        overrides: &AgentOverrides,
        parent_session: &str,
    ) -> Result<(Agent, String), Box<dyn Error + Send + Sync>> {
        self.calls.lock().unwrap().push(Call {
            base: base.to_string(),
            instructions: overrides.instructions.clone(),
            tools: overrides.tools.clone(),
            parent_session: parent_session.to_string(),
        });
        self.spawn(base, overrides, parent_session).await
    }
}
