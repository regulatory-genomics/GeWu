use anyhow::{Result, anyhow};
use futures::{Stream, StreamExt, pin_mut};
use gewu_core::agent::{Agent, IdleAction, Steering};
use gewu_core::types::AgentEvent;

pub struct TaskLoop {
    agent: Agent,
    inbox: Steering,
}

#[derive(Clone)]
pub struct TaskSteering(Steering);

impl TaskSteering {
    pub fn send(&self, message: impl Into<String>) -> std::result::Result<(), String> {
        self.0.send(message).map_err(|error| error.0)
    }
}

impl TaskLoop {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            inbox: Steering::new(),
        }
    }

    pub fn steering(&self) -> TaskSteering {
        TaskSteering(self.inbox.clone())
    }

    pub async fn run(self, prompt: &str) -> Result<String> {
        self.run_with_events(prompt, |_| {}).await
    }

    pub async fn run_with_events(
        mut self,
        prompt: &str,
        mut on_event: impl FnMut(&AgentEvent),
    ) -> Result<String> {
        let mut response = finish(
            self.agent.ask_with_inbox(prompt, self.inbox.clone()),
            &mut on_event,
        )
        .await?;

        loop {
            match self.inbox.idle_action() {
                IdleAction::Resume => {
                    response = finish(
                        self.agent.resume_with_inbox(self.inbox.clone()),
                        &mut on_event,
                    )
                    .await?;
                }
                IdleAction::Wait => self.inbox.notified().await,
                IdleAction::Finish => return Ok(response),
            }
        }
    }
}

impl Drop for TaskLoop {
    fn drop(&mut self) {
        self.inbox.close();
    }
}

async fn finish(
    events: impl Stream<Item = Result<AgentEvent>>,
    on_event: &mut impl FnMut(&AgentEvent),
) -> Result<String> {
    pin_mut!(events);
    let mut response = None;
    while let Some(event) = events.next().await {
        let event = event?;
        on_event(&event);
        if let AgentEvent::Finish { response: text } = event {
            response = Some(text);
        }
    }
    response.ok_or_else(|| anyhow!("agent stream ended without a final result"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};
    use gewu_core::agent::BackgroundNotification;
    use gewu_core::traits::{LlmProvider, Tool};
    use gewu_core::types::{Message, ProviderEvent};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    type MockResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    struct ScriptedProvider(Mutex<std::vec::IntoIter<&'static str>>);

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn build(&self, _: &[Message], _: &[Arc<dyn Tool>]) -> MockResult<Value> {
            Ok(Value::Null)
        }

        async fn send<'a>(
            &'a self,
            _: Value,
        ) -> MockResult<BoxStream<'a, MockResult<ProviderEvent>>> {
            let text = self.0.lock().unwrap().next().ok_or("no response")?;
            Ok(stream::iter([
                Ok(ProviderEvent::ContentDelta { delta: text.into() }),
                Ok(ProviderEvent::Finish { reason: None }),
            ])
            .boxed())
        }

        async fn list_models(&self) -> MockResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    pub(crate) fn coordinator(responses: Vec<&'static str>) -> TaskLoop {
        TaskLoop::new(Agent::builder().llm(scripted(responses)).build().unwrap())
    }

    /// Returns a provider that replies with the given texts in order.
    pub(crate) fn scripted(responses: Vec<&'static str>) -> Arc<dyn LlmProvider> {
        Arc::new(ScriptedProvider(Mutex::new(responses.into_iter())))
    }

    #[tokio::test]
    async fn late_steering_triggers_another_round() {
        let task_loop = coordinator(vec!["first", "revised"]);
        let steering = task_loop.steering();
        let output = task_loop
            .run_with_events("start", |event| {
                if matches!(event, AgentEvent::Finish { response } if response == "first") {
                    steering.send("revise").unwrap();
                }
            })
            .await
            .unwrap();
        assert_eq!(output, "revised");
        assert_eq!(steering.send("too late"), Err("too late".into()));
    }

    #[tokio::test]
    async fn background_result_resumes_idle_agent_and_waits_for_all_jobs() {
        let task_loop = coordinator(vec!["first", "revised"]);
        let inbox = task_loop.inbox.clone();
        let job = inbox.begin_background_job().unwrap();
        let remaining = inbox.begin_background_job().unwrap();
        let mut run = Box::pin(task_loop.run("start"));
        assert!(futures::poll!(&mut run).is_pending());
        inbox.notify(BackgroundNotification {
            source: "test".into(),
            id: "job".into(),
            status: "completed".into(),
            output: "data".into(),
        });
        drop(job);
        assert!(futures::poll!(&mut run).is_pending());
        assert!(!inbox.is_pending());
        drop(remaining);
        assert_eq!(run.await.unwrap(), "revised");
    }

    #[tokio::test]
    async fn stream_failure_supersedes_finish() {
        let events = stream::iter([
            Ok(AgentEvent::Finish {
                response: "provisional".into(),
            }),
            Err(anyhow!("stream failed")),
        ]);
        assert_eq!(
            finish(events, &mut |_| {}).await.unwrap_err().to_string(),
            "stream failed"
        );
        assert!(finish(stream::empty(), &mut |_| {}).await.is_err());
    }
}
