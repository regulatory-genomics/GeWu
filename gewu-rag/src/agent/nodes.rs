use crate::agent::prompts;
use crate::agent::state::RagQueryResult;
use crate::agent::state::{AgentState, MainState, PartialAgentState, PartialMainState};
use crate::config::Config;
use anyhow::Result;
use futures::StreamExt;
use gewu_core::types::{ContentPart, ProviderEvent, ToolCall, ToolContext};
use gewu_core::{Message, Tool};
use gewu_graph::CompiledGraph;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static NODE_INVOCATION_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_invocation_id() -> usize {
    NODE_INVOCATION_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn get_message_content(msg: &Message) -> String {
    match msg {
        Message::User { content } => content
            .iter()
            .map(|part| match part {
                gewu_core::types::ContentPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(""),
        Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

pub async fn summarize_history(
    state: MainState,
    llm: Arc<dyn gewu_core::LlmProvider>,
) -> PartialMainState {
    let last_message = state.messages.last();
    let content = last_message.map(get_message_content).unwrap_or_default();

    if content.is_empty() {
        return PartialMainState {
            conversation_summary: Some(String::new()),
            ..Default::default()
        };
    }

    let compression_prompt = prompts::get_aggregation_prompt();
    let full_prompt = format!(
        "{}\n\nOriginal conversation:\n{}",
        compression_prompt, content
    );

    let messages = vec![
        Message::System {
            content: full_prompt,
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "Summarize the conversation history".to_string(),
            }],
        },
    ];

    let mut stream = llm
        .stream_response(&messages, &[])
        .await
        .unwrap_or_else(|_| {
            Box::pin(futures::stream::once(async {
                Ok(ProviderEvent::Error {
                    error: "LLM error".to_string(),
                })
            }))
        });

    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(ProviderEvent::ContentDelta { delta }) = event {
            summary.push_str(&delta);
        }
    }

    PartialMainState {
        conversation_summary: Some(summary),
        ..Default::default()
    }
}

pub async fn rewrite_query(
    state: MainState,
    llm: Arc<dyn gewu_core::LlmProvider>,
) -> PartialMainState {
    let last_message = state.messages.last();
    let content = last_message.map(get_message_content).unwrap_or_default();

    if content.is_empty() {
        return PartialMainState {
            question_is_clear: Some(false),
            clarification_needed: Some("No message content".to_string()),
            ..Default::default()
        };
    }

    let conversation_section = if state.conversation_summary.is_empty() {
        String::new()
    } else {
        format!("Conversation Context:\n{}\n", state.conversation_summary)
    };

    let rewrite_prompt = prompts::get_rewrite_query_prompt();
    let full_prompt = format!(
        "{}\n\n{}\n\nUser Query:\n{}",
        conversation_section, rewrite_prompt, content
    );

    let messages = vec![
        Message::System {
            content: full_prompt,
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: content.clone(),
            }],
        },
    ];

    let mut stream = llm
        .stream_response(&messages, &[])
        .await
        .unwrap_or_else(|_| {
            Box::pin(futures::stream::once(async {
                Ok(ProviderEvent::Error {
                    error: "LLM error".to_string(),
                })
            }))
        });

    let mut rewritten_query = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(ProviderEvent::ContentDelta { delta }) = event {
            rewritten_query.push_str(&delta);
        }
    }

    let rewritten_trimmed = rewritten_query.trim().to_string();
    let rewritten_lower = rewritten_trimmed.to_lowercase();

    let is_header_only = rewritten_trimmed.lines().all(|l| {
        let t = l.trim();
        t.is_empty()
            || t.starts_with("**")
            || t.starts_with("#")
            || t.starts_with("*")
            || t.starts_with("=")
            || t.to_lowercase().starts_with("rewritten")
    });

    let has_meaningful_content = rewritten_trimmed.len() > 10 && !is_header_only;

    let rewritten_is_clear = has_meaningful_content
        && !rewritten_lower.contains("unclear")
        && !rewritten_lower.contains("clarification");

    let single_line = rewritten_trimmed.lines().count() <= 1 && rewritten_trimmed.len() > 2;

    if rewritten_is_clear || single_line {
        let questions: Vec<String> = if single_line && !rewritten_trimmed.starts_with("**") {
            vec![rewritten_trimmed.clone()]
        } else {
            rewritten_trimmed
                .lines()
                .filter(|l| {
                    let trimmed = l.trim();
                    !trimmed.is_empty()
                        && trimmed.len() > 5
                        && !trimmed.starts_with("**")
                        && !trimmed.starts_with("#")
                        && !trimmed.starts_with("*")
                        && !trimmed.starts_with("-")
                        && !trimmed.starts_with("=")
                        && !trimmed.to_lowercase().starts_with("rewritten")
                        && !trimmed.to_lowercase().starts_with("preserve")
                        && !trimmed.to_lowercase().starts_with("made")
                        && !trimmed.to_lowercase().starts_with("fix")
                        && !trimmed.to_lowercase().starts_with("query")
                })
                .map(|l| l.trim().to_string())
                .take(3)
                .collect()
        };

        if !questions.is_empty() {
            return PartialMainState {
                question_is_clear: Some(true),
                original_query: Some(content.clone()),
                rewritten_questions: Some(questions),
                messages: Some(vec![]),
                ..Default::default()
            };
        }
    }

    let original_is_simple = content.len() > 2 && !content.contains('\n');
    if original_is_simple && content.len() < 100 {
        return PartialMainState {
            question_is_clear: Some(true),
            original_query: Some(content.clone()),
            rewritten_questions: Some(vec![content.clone()]),
            messages: Some(vec![]),
            ..Default::default()
        };
    }

    PartialMainState {
        question_is_clear: Some(false),
        clarification_needed: Some(
            "I need more information to understand your question. Could you please clarify?"
                .to_string(),
        ),
        ..Default::default()
    }
}

pub async fn orchestrator(
    state: AgentState,
    llm: Arc<dyn gewu_core::LlmProvider>,
    tools: Vec<Arc<dyn Tool>>,
    _config: &Config,
) -> PartialAgentState {
    let invocation_id = next_invocation_id();
    tracing::info!(
        "[NODE #{}] orchestrator starting: question={}, msgs={}, tool_count={}, iteration={}, retrieval_keys={}",
        invocation_id,
        state.question,
        state.messages.len(),
        state.tool_call_count,
        state.iteration_count,
        state.retrieval_keys.len()
    );

    let sys_msg = Message::System {
        content: prompts::get_orchestrator_prompt(),
    };

    let mut all_messages = vec![sys_msg];

    if !state.context_summary.is_empty() {
        all_messages.push(Message::User {
            content: vec![ContentPart::Text {
                text: format!(
                    "[COMPRESSED CONTEXT FROM PRIOR RESEARCH]\n\n{}",
                    state.context_summary
                ),
            }],
        });
    }

    if state.messages.is_empty() {
        all_messages.push(Message::User {
            content: vec![ContentPart::Text {
                text: state.question.clone(),
            }],
        });
        all_messages.push(Message::User {
            content: vec![ContentPart::Text {
                text:
                    "YOU MUST CALL 'search_child_chunks' AS THE FIRST STEP TO ANSWER THIS QUESTION."
                        .to_string(),
            }],
        });
    } else {
        all_messages.extend(state.messages.clone());
    }

    let stream = llm.stream_response(&all_messages, &tools).await;
    if let Err(e) = stream {
        tracing::error!(
            "[NODE #{}] orchestrator: stream_response error: {:?}",
            invocation_id,
            e
        );
        return PartialAgentState::default();
    }
    let mut stream = stream.unwrap();

    let mut new_messages = Vec::new();
    let mut current_content = String::new();
    let mut current_tool_calls: Option<Vec<ToolCall>> = None;

    while let Some(event) = stream.next().await {
        if let Ok(event) = event {
            match event {
                ProviderEvent::ContentDelta { delta } => {
                    current_content.push_str(&delta);
                }
                ProviderEvent::ToolCallStart { id, name } => {
                    tracing::info!("[TOOL_CALL_START] id={}, name={}", id, name);
                    current_tool_calls = Some(vec![ToolCall {
                        id,
                        r#type: "function".to_string(),
                        function: gewu_core::types::FunctionCall {
                            name,
                            arguments: String::new(),
                        },
                    }]);
                }
                ProviderEvent::ToolCallDelta { id, delta } => {
                    tracing::debug!("[TOOL_CALL_DELTA] id={}, delta_len={}", id, delta.len());
                    if let Some(ref mut calls) = current_tool_calls
                        && let Some(call) = calls.iter_mut().find(|c| c.id == id)
                    {
                        call.function.arguments.push_str(&delta);
                    }
                }
                ProviderEvent::Finish { reason } => {
                    tracing::info!("[LLM_FINISH] reason={:?}", reason);
                    tracing::debug!(
                        "[LLM_FINISH] content_len={}, tool_calls={:?}",
                        current_content.len(),
                        current_tool_calls.as_ref().map(|c| c.len()).unwrap_or(0)
                    );
                    if !current_content.is_empty() || current_tool_calls.is_some() {
                        if !current_content.is_empty() {
                            tracing::info!(
                                "[LLM_CONTENT_PREVIEW] {}",
                                &current_content[..current_content.len().min(200)]
                            );
                        }
                        if let Some(ref tcs) = current_tool_calls {
                            for tc in tcs {
                                tracing::info!(
                                    "[LLM_TOOL_CALL] name={}, id={}, args={}",
                                    tc.function.name,
                                    tc.id,
                                    tc.function.arguments
                                );
                            }
                        }
                        new_messages.push(Message::Assistant {
                            content: if current_content.is_empty() {
                                None
                            } else {
                                Some(current_content.clone())
                            },
                            tool_calls: current_tool_calls.clone(),
                        });
                        current_content.clear();
                        current_tool_calls = None;
                    }
                }
                ProviderEvent::Error { error } => {
                    tracing::error!("[LLM_ERROR] error={}", error);
                }
                _ => {
                    tracing::trace!("[LLM_EVENT] unhandled: {:?}", event);
                }
            }
        }
    }

    if !current_content.is_empty() || current_tool_calls.is_some() {
        new_messages.push(Message::Assistant {
            content: if current_content.is_empty() {
                None
            } else {
                Some(current_content.clone())
            },
            tool_calls: current_tool_calls,
        });
    }

    let tool_calls = if let Some(Message::Assistant { tool_calls, .. }) = new_messages.last() {
        tool_calls.clone().unwrap_or_default()
    } else {
        vec![]
    };

    let iteration_increment = if !tool_calls.is_empty() { 1 } else { 0 };

    PartialAgentState {
        tool_call_count: Some(0),
        iteration_count: Some(iteration_increment),
        messages: Some(new_messages),
        ..Default::default()
    }
}

pub async fn execute_tools(state: AgentState, tools: Vec<Arc<dyn Tool>>) -> PartialAgentState {
    let invocation_id = next_invocation_id();
    tracing::info!(
        "[NODE #{}] execute_tools starting: tool_count={}, iteration={}, msgs={}",
        invocation_id,
        state.tool_call_count,
        state.iteration_count,
        state.messages.len()
    );

    let last_message = state.messages.last();

    let tool_calls = if let Some(Message::Assistant { tool_calls, .. }) = last_message {
        tool_calls.clone().unwrap_or_default()
    } else {
        vec![]
    };

    if tool_calls.is_empty() {
        return PartialAgentState::default();
    }

    let mut new_messages = state.messages.clone();

    let ctx = ToolContext::new(
        format!("agent-{}", state.question_index),
        "main".to_string(),
    );

    for tc in &tool_calls {
        let tool_name = &tc.function.name;
        tracing::info!(
            "[TOOL_EXECUTE] name={}, id={}, args={}",
            tool_name,
            tc.id,
            tc.function.arguments
        );
        let args_result: Result<serde_json::Value, _> =
            serde_json::from_str(&tc.function.arguments);

        if let Ok(args) = args_result {
            let tool = tools.iter().find(|t| t.name() == tool_name);

            if let Some(tool) = tool {
                match tool.execute(args, &ctx).await {
                    Ok(result) => {
                        tracing::info!(
                            "[TOOL_RESULT] name={}, id={}, output_len={}",
                            tool_name,
                            tc.id,
                            result.output.len()
                        );
                        tracing::debug!(
                            "[TOOL_RESULT_PREVIEW] {}",
                            &result.output[..result.output.len().min(300)]
                        );
                        new_messages.push(Message::Tool {
                            tool_call_id: tc.id.clone(),
                            tool_name: Some(tool_name.clone()),
                            content: result.output.clone(),
                            media: vec![],
                        });
                    }
                    Err(e) => {
                        new_messages.push(Message::Tool {
                            tool_call_id: tc.id.clone(),
                            tool_name: Some(tool_name.clone()),
                            content: format!("Error: {}", e),
                            media: vec![],
                        });
                    }
                }
            } else {
                new_messages.push(Message::Tool {
                    tool_call_id: tc.id.clone(),
                    tool_name: Some(tool_name.clone()),
                    content: format!("Tool '{}' not found", tool_name),
                    media: vec![],
                });
            }
        } else {
            new_messages.push(Message::Tool {
                tool_call_id: tc.id.clone(),
                tool_name: Some(tool_name.clone()),
                content: format!("Failed to parse arguments: {}", tc.function.arguments),
                media: vec![],
            });
        }
    }

    tracing::info!(
        "[NODE #{}] execute_tools done: returning tool_call_count={} (tools_executed={}), iteration=1, new_msgs={}",
        invocation_id,
        tool_calls.len(),
        tool_calls.len(),
        new_messages.len() - state.messages.len()
    );

    PartialAgentState {
        messages: Some(new_messages),
        tool_call_count: Some(tool_calls.len() as i32),
        iteration_count: Some(1),
        ..Default::default()
    }
}

pub async fn compress_context(
    state: AgentState,
    llm: Arc<dyn gewu_core::LlmProvider>,
) -> PartialAgentState {
    tracing::info!(
        "[NODE] compress_context starting: msgs={}, summary_len={}",
        state.messages.len(),
        state.context_summary.len()
    );

    let content = state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => {
                content.as_ref().filter(|c| !c.is_empty()).cloned()
            }
            Message::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    tracing::debug!(
        "[NODE] compress_context content to compress: len={}",
        content.len()
    );

    let compression_prompt = prompts::get_context_compression_prompt();
    let full_prompt = format!("{}\n\nOriginal context:\n{}", compression_prompt, content);

    let messages = vec![
        Message::System {
            content: full_prompt,
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "Compress this context".to_string(),
            }],
        },
    ];

    let mut stream = llm
        .stream_response(&messages, &[])
        .await
        .unwrap_or_else(|_| {
            Box::pin(futures::stream::once(async {
                Ok(ProviderEvent::Error {
                    error: "LLM error".to_string(),
                })
            }))
        });

    let mut compressed = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(ProviderEvent::ContentDelta { delta }) = event {
            compressed.push_str(&delta);
        }
    }

    tracing::info!(
        "[NODE] compress_context done: compressed_len={}",
        compressed.len()
    );
    PartialAgentState {
        context_summary: Some(compressed),
        ..Default::default()
    }
}

pub async fn fallback_response(
    state: AgentState,
    _llm: Arc<dyn gewu_core::LlmProvider>,
) -> PartialAgentState {
    let mut all_content = Vec::new();
    let mut tool_count = 0;
    let mut msg_types = Vec::new();

    for m in state.messages.iter().rev() {
        msg_types.push(format!("{:?}", m));
        match m {
            Message::Assistant { content, .. } => {
                if let Some(c) = content
                    && !c.is_empty()
                {
                    all_content.push(c.clone());
                }
            }
            Message::Tool { content, .. } => {
                tool_count += 1;
                if !content.is_empty() {
                    all_content.push(content.clone());
                }
            }
            _ => {}
        }
    }

    tracing::debug!(
        "fallback_response: tool_count={}, content_len={}, msg_types={}",
        tool_count,
        all_content.len(),
        msg_types.len()
    );

    let answer = if all_content.is_empty() {
        "I couldn't find relevant information to answer your question.".to_string()
    } else {
        all_content.join("\n\n")
    };

    let sources = Vec::new();

    let result = RagQueryResult {
        index: state.question_index,
        question: state.question.clone(),
        answer: answer.clone(),
        sources,
    };

    PartialAgentState {
        final_answer: Some(answer),
        agent_answers: Some(vec![result]),
        ..Default::default()
    }
}

pub async fn collect_answer(state: AgentState) -> PartialAgentState {
    tracing::info!(
        "[NODE] collect_answer starting: messages={}",
        state.messages.len()
    );
    for (i, m) in state.messages.iter().enumerate() {
        match m {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                tracing::debug!(
                    "  msg[{}]: Assistant content_len={} tool_calls={}",
                    i,
                    content.as_ref().map(|c| c.len()).unwrap_or(0),
                    tool_calls.as_ref().map(|t| t.len()).unwrap_or(0)
                );
            }
            Message::Tool { content, .. } => {
                tracing::debug!("  msg[{}]: Tool content_len={}", i, content.len());
            }
            _ => {
                tracing::debug!("  msg[{}]: Other", i);
            }
        }
    }

    let last_assistant_with_content = state.messages.iter().rev().find_map(|m| {
        if let Message::Assistant {
            content,
            tool_calls,
        } = m
        {
            tracing::info!(
                "[NODE] collect_answer checking Assistant: content_len={}, tool_calls={}",
                content.as_ref().map(|c| c.len()).unwrap_or(0),
                tool_calls.as_ref().map(|t| t.len()).unwrap_or(0)
            );
            if content.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
                && tool_calls.as_ref().map(|t| t.is_empty()).unwrap_or(true)
            {
                tracing::info!(
                    "[NODE] collect_answer found assistant content with no tool_calls, len={}",
                    content.as_ref().map(|c| c.len()).unwrap_or(0)
                );
                Some(content.clone().unwrap_or_default())
            } else {
                None
            }
        } else {
            None
        }
    });

    let answer = if let Some(content) = last_assistant_with_content {
        content
    } else {
        state
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let Message::Tool { content, .. } = m {
                    if !content.is_empty() {
                        Some(content.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                "I couldn't find relevant information to answer your question.".to_string()
            })
    };

    let sources = extract_sources(&answer);

    let result = RagQueryResult {
        index: state.question_index,
        question: state.question.clone(),
        answer: answer.clone(),
        sources,
    };

    PartialAgentState {
        final_answer: Some(answer),
        agent_answers: Some(vec![result]),
        ..Default::default()
    }
}

pub async fn run_parallel_agents(
    state: MainState,
    _llm: Arc<dyn gewu_core::LlmProvider>,
    agent_graph: CompiledGraph<AgentState>,
) -> PartialMainState {
    if state.rewritten_questions.is_empty() {
        return PartialMainState {
            messages: Some(vec![Message::Assistant {
                content: Some("No questions to answer.".to_string()),
                tool_calls: None,
            }]),
            agent_answers: Some(vec![]),
            ..Default::default()
        };
    }

    let mut all_answers = Vec::new();

    let agent_graphs: Vec<_> = state
        .rewritten_questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let agent_state = AgentState::new(q.clone(), i);
            let graph = agent_graph.clone();
            async move { graph.ainvoke(agent_state).await }
        })
        .collect();

    let results = futures::future::join_all(agent_graphs).await;

    for result in results {
        match result {
            Ok(agent_result) => {
                tracing::info!(
                    "[NODE] run_parallel_agents: got agent_result with agent_answers={}",
                    agent_result.agent_answers.len()
                );
                all_answers.extend(agent_result.agent_answers);
            }
            Err(e) => {
                tracing::error!("Agent graph error: {:?}", e);
            }
        }
    }

    tracing::info!(
        "[NODE] run_parallel_agents: returning {} total answers",
        all_answers.len()
    );

    PartialMainState {
        agent_answers: Some(all_answers),
        ..Default::default()
    }
}

pub async fn aggregate_answers(
    state: MainState,
    llm: Arc<dyn gewu_core::LlmProvider>,
) -> PartialMainState {
    tracing::info!(
        "[NODE] aggregate_answers starting: agent_answers={}",
        state.agent_answers.len()
    );

    if state.agent_answers.is_empty() {
        tracing::info!("[NODE] aggregate_answers: no agent answers, returning default");
        return PartialMainState {
            messages: Some(vec![Message::Assistant {
                content: Some("No answers were generated.".to_string()),
                tool_calls: None,
            }]),
            ..Default::default()
        };
    }

    let mut sorted_answers = state.agent_answers.clone();
    sorted_answers.sort_by_key(|a| a.index);

    for (i, ans) in sorted_answers.iter().enumerate() {
        tracing::info!(
            "[NODE] aggregate_answers: answer[{}] index={}, answer_len={}",
            i,
            ans.index,
            ans.answer.len()
        );
        if ans.answer.len() < 200 {
            tracing::info!(
                "[NODE] aggregate_answers: answer[{}] content: {}",
                i,
                ans.answer
            );
        }
    }

    let formatted_answers = sorted_answers
        .iter()
        .enumerate()
        .map(|(i, ans)| format!("\nAnswer {}:\n{}\n", i + 1, ans.answer))
        .collect::<String>();

    let user_message = format!(
        "Original user question: {}\n\nRetrieved answers:{}",
        state.original_query, formatted_answers
    );

    tracing::info!(
        "[NODE] aggregate_answers: user_message len={}",
        user_message.len()
    );

    let messages = vec![
        Message::System {
            content: prompts::get_aggregation_prompt(),
        },
        Message::User {
            content: vec![ContentPart::Text { text: user_message }],
        },
    ];

    let mut stream = llm
        .stream_response(&messages, &[])
        .await
        .unwrap_or_else(|_| {
            Box::pin(futures::stream::once(async {
                Ok(ProviderEvent::Error {
                    error: "LLM error".to_string(),
                })
            }))
        });

    let mut response = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(ProviderEvent::ContentDelta { delta }) = event {
            response.push_str(&delta);
        } else if let Ok(ProviderEvent::Error { error }) = event {
            tracing::error!("[NODE] aggregate_answers LLM error: {}", error);
        }
    }

    tracing::info!(
        "[NODE] aggregate_answers: LLM response len={}",
        response.len()
    );
    if response.is_empty() {
        tracing::warn!("[NODE] aggregate_answers: LLM returned empty response!");
    } else if response.len() < 200 {
        tracing::info!("[NODE] aggregate_answers: LLM response: {}", response);
    }

    PartialMainState {
        messages: Some(vec![Message::Assistant {
            content: Some(response),
            tool_calls: None,
        }]),
        ..Default::default()
    }
}

fn extract_sources(answer: &str) -> Vec<String> {
    let mut sources = Vec::new();
    let answer_lower = answer.to_lowercase();

    if let Some(sources_pos) = answer_lower.find("**sources:**") {
        let sources_section = &answer[sources_pos..];
        for line in sources_section.lines().skip(1) {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("**") {
                sources.push(trimmed.to_string());
            }
        }
    }

    sources
}
