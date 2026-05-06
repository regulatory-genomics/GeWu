use crate::agent::state::{AgentState, MainState};
use crate::config::Config;
use gewu_core::Message;

pub fn route_after_rewrite(state: &MainState) -> String {
    if state.question_is_clear {
        "run_agents".to_string()
    } else {
        "request_clarification".to_string()
    }
}

pub fn route_after_orchestrator(state: &AgentState, config: &Config) -> String {
    let iteration = state.iteration_count;
    let tool_count = state.tool_call_count;

    let last_message = state.messages.last();
    let (content_len, has_tool_calls) = if let Some(Message::Assistant {
        content,
        tool_calls,
    }) = last_message
    {
        (
            content.as_ref().map(|c| c.len()).unwrap_or(0),
            tool_calls.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
        )
    } else {
        (0, false)
    };

    tracing::info!(
        "[ROUTE orchestrator] iter={}/max={}, tool_count={}/{}, msgs={}, content_len={}, has_tool_calls={}",
        iteration,
        config.max_iterations,
        tool_count,
        config.max_tool_calls,
        state.messages.len(),
        content_len,
        has_tool_calls
    );

    if iteration >= config.max_iterations as i32 || tool_count > config.max_tool_calls as i32 {
        tracing::info!("[ROUTE] -> fallback_response (limits hit)");
        return "fallback_response".to_string();
    }

    if let Some(Message::Assistant {
        content,
        tool_calls,
    }) = last_message
    {
        if let Some(tcs) = tool_calls
            && !tcs.is_empty()
        {
            if tool_count >= 2 {
                tracing::info!("[ROUTE] -> collect_answer (tool_count >= 2)");
                return "collect_answer".to_string();
            }
            let has_substantive_content = content.as_ref().map(|c| c.len()).unwrap_or(0) > 100;
            if has_substantive_content {
                tracing::info!(
                    "[ROUTE] -> collect_answer (has substantive content len={})",
                    content.as_ref().map(|c| c.len()).unwrap_or(0)
                );
                return "collect_answer".to_string();
            }
            tracing::info!("[ROUTE] -> tools");
            return "tools".to_string();
        }
        if content.as_ref().map(|c| !c.is_empty()).unwrap_or(false) {
            tracing::info!("[ROUTE] -> collect_answer (has content)");
            return "collect_answer".to_string();
        }
    }

    tracing::info!("[ROUTE] -> fallback_response (default)");
    "fallback_response".to_string()
}

pub fn route_after_tools(state: &AgentState, config: &Config) -> String {
    let last_assistant = state.messages.iter().rev().find_map(|m| {
        if let Message::Assistant {
            content,
            tool_calls,
        } = m
        {
            Some((content.clone(), tool_calls.clone()))
        } else {
            None
        }
    });

    let current_tokens = estimate_tokens(state);

    let max_allowed = config.base_token_threshold as i32
        + (state.context_summary.len() as f64 * config.token_growth_factor) as i32;

    if let Some((content, tool_calls)) = last_assistant {
        let has_tool_calls = tool_calls.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
        let content_len = content.as_ref().map(|c| c.len()).unwrap_or(0);
        let tool_calls_len = tool_calls.as_ref().map(|t| t.len()).unwrap_or(0);

        tracing::info!(
            "[ROUTE tools->] tool_count={}, content_len={}, has_tool_calls={}, tool_calls_len={}, tokens={}/{}",
            state.tool_call_count,
            content_len,
            has_tool_calls,
            tool_calls_len,
            current_tokens,
            max_allowed
        );

        if !has_tool_calls {
            tracing::info!("[ROUTE] -> collect_answer (no tool_calls)");
            return "collect_answer".to_string();
        }

        if state.tool_call_count >= 2 {
            tracing::info!(
                "[ROUTE] -> collect_answer (tool_count >= 2) [CHECKING: {} >= 2]",
                state.tool_call_count
            );
            return "collect_answer".to_string();
        }
    } else {
        tracing::info!(
            "[ROUTE tools->] no assistant msg found, tool_count={}, tokens={}/{}",
            state.tool_call_count,
            current_tokens,
            max_allowed
        );
    }

    if current_tokens > max_allowed {
        tracing::info!("[ROUTE] -> compress_context (tokens > limit)");
        "compress_context".to_string()
    } else {
        tracing::info!("[ROUTE] -> orchestrator");
        "orchestrator".to_string()
    }
}

fn estimate_tokens(state: &AgentState) -> i32 {
    let messages_tokens = state
        .messages
        .iter()
        .map(|m| match m {
            Message::System { content } => content.len(),
            Message::User { content } => content
                .iter()
                .map(|part| match part {
                    gewu_core::types::ContentPart::Text { text } => text.len(),
                    _ => 10,
                })
                .sum::<usize>(),
            Message::Assistant { content, .. } => content.as_ref().map(|c| c.len()).unwrap_or(0),
            Message::Tool { content, .. } => content.len(),
        })
        .sum::<usize>() as i32;

    let summary_tokens = (state.context_summary.len() / 4) as i32;

    messages_tokens + summary_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_after_rewrite_clear() {
        let state = MainState {
            question_is_clear: true,
            ..Default::default()
        };

        let result = route_after_rewrite(&state);
        assert_eq!(result, "run_agents");
    }

    #[test]
    fn test_route_after_rewrite_not_clear() {
        let state = MainState {
            question_is_clear: false,
            clarification_needed: Some("Need clarification".to_string()),
            ..Default::default()
        };

        let result = route_after_rewrite(&state);
        assert_eq!(result, "request_clarification");
    }

    #[test]
    fn test_estimate_tokens() {
        let state = AgentState::default();
        let tokens = estimate_tokens(&state);
        assert_eq!(tokens, 0);
    }
}
