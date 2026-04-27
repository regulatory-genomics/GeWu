use anyhow::Result;
use gewu_graph::graph::{CompiledGraph, NodeResult, StateGraph};
use gewu_graph::State;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AgentState {
    provider: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    user_input: String,
    intent: Option<String>,
    specialized_response: Option<String>,
    final_response: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PartialAgentState {
    provider: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    user_input: Option<String>,
    intent: Option<String>,
    specialized_response: Option<String>,
    final_response: Option<String>,
    error: Option<String>,
}

impl State for AgentState {
    type Partial = PartialAgentState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            provider: update.provider.or_else(|| self.provider.clone()),
            api_key: update.api_key.or_else(|| self.api_key.clone()),
            model: update.model.or_else(|| self.model.clone()),
            user_input: update.user_input.unwrap_or_else(|| self.user_input.clone()),
            intent: update.intent.or_else(|| self.intent.clone()),
            specialized_response: update
                .specialized_response
                .or_else(|| self.specialized_response.clone()),
            final_response: update
                .final_response
                .unwrap_or_else(|| self.final_response.clone()),
            error: update.error.or_else(|| self.error.clone()),
        }
    }
}

async fn call_llm(
    provider: &str,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let full_api_key = if api_key.starts_with("sk-") {
        api_key.to_string()
    } else {
        format!("sk-{}", api_key)
    };

    let client = reqwest::Client::new();
    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "stream": false
    });

    let response = client
        .post(format!("{}/chat/completions", provider))
        .header("Authorization", format!("Bearer {}", full_api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let json: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(content)
}

async fn classify_intent(state: AgentState) -> PartialAgentState {
    let provider = match state.provider.as_ref() {
        Some(p) => p,
        None => return PartialAgentState { error: Some("Provider not set".to_string()), ..Default::default() },
    };
    let api_key = match state.api_key.as_ref() {
        Some(k) => k,
        None => return PartialAgentState { error: Some("API key not set".to_string()), ..Default::default() },
    };
    let model = match state.model.as_ref() {
        Some(m) => m,
        None => return PartialAgentState { error: Some("Model not set".to_string()), ..Default::default() },
    };

    let system = r#"You are an intent classifier. Analyze the user input and classify it into one of three categories:
- "code" if the user is asking for help with programming, code review, debugging, or technical implementation
- "qa" if the user is asking a knowledge question, factual inquiry, or explanation request
- "general" if the input doesn't fit the above categories

Respond with ONLY a single word: either "code", "qa", or "general"."#;

    match call_llm(provider, api_key, model, system, &state.user_input).await {
        Ok(mut intent) => {
            intent = intent.trim().to_lowercase();
            let intent = if ["code", "qa", "general"].contains(&intent.as_str()) {
                intent
            } else {
                "general".to_string()
            };
            PartialAgentState {
                intent: Some(intent),
                ..Default::default()
            }
        }
        Err(e) => PartialAgentState {
            error: Some(e),
            ..Default::default()
        },
    }
}

async fn code_assist(state: AgentState) -> PartialAgentState {
    let provider = match state.provider.as_ref() {
        Some(p) => p,
        None => return PartialAgentState { error: Some("Provider not set".to_string()), ..Default::default() },
    };
    let api_key = match state.api_key.as_ref() {
        Some(k) => k,
        None => return PartialAgentState { error: Some("API key not set".to_string()), ..Default::default() },
    };
    let model = match state.model.as_ref() {
        Some(m) => m,
        None => return PartialAgentState { error: Some("Model not set".to_string()), ..Default::default() },
    };

    let system = r#"You are an expert programming assistant. Help the user with their coding task.
Provide clear, concise code examples and explanations. Focus on best practices and practical solutions."#;

    match call_llm(provider, api_key, model, system, &state.user_input).await {
        Ok(response) => PartialAgentState {
            specialized_response: Some(response),
            ..Default::default()
        },
        Err(e) => PartialAgentState {
            error: Some(e),
            ..Default::default()
        },
    }
}

async fn qa_assist(state: AgentState) -> PartialAgentState {
    let provider = match state.provider.as_ref() {
        Some(p) => p,
        None => return PartialAgentState { error: Some("Provider not set".to_string()), ..Default::default() },
    };
    let api_key = match state.api_key.as_ref() {
        Some(k) => k,
        None => return PartialAgentState { error: Some("API key not set".to_string()), ..Default::default() },
    };
    let model = match state.model.as_ref() {
        Some(m) => m,
        None => return PartialAgentState { error: Some("Model not set".to_string()), ..Default::default() },
    };

    let system = r#"You are a knowledgeable Q&A assistant. Answer the user's question with accurate,
well-researched information. Provide context and explanations where helpful."#;

    match call_llm(provider, api_key, model, system, &state.user_input).await {
        Ok(response) => PartialAgentState {
            specialized_response: Some(response),
            ..Default::default()
        },
        Err(e) => PartialAgentState {
            error: Some(e),
            ..Default::default()
        },
    }
}

async fn general_assist(state: AgentState) -> PartialAgentState {
    let provider = match state.provider.as_ref() {
        Some(p) => p,
        None => return PartialAgentState { error: Some("Provider not set".to_string()), ..Default::default() },
    };
    let api_key = match state.api_key.as_ref() {
        Some(k) => k,
        None => return PartialAgentState { error: Some("API key not set".to_string()), ..Default::default() },
    };
    let model = match state.model.as_ref() {
        Some(m) => m,
        None => return PartialAgentState { error: Some("Model not set".to_string()), ..Default::default() },
    };

    let system = r#"You are a helpful conversational assistant. Engage with the user in a friendly
and informative manner. Provide helpful responses to their queries."#;

    match call_llm(provider, api_key, model, system, &state.user_input).await {
        Ok(response) => PartialAgentState {
            specialized_response: Some(response),
            ..Default::default()
        },
        Err(e) => PartialAgentState {
            error: Some(e),
            ..Default::default()
        },
    }
}

async fn synthesizer(state: AgentState) -> PartialAgentState {
    let specialized = state
        .specialized_response
        .clone()
        .unwrap_or_else(|| "No response generated.".to_string());

    let final_response = format!(
        "[{} mode]\n\n{}\n\n[End of response]",
        state.intent.as_deref().unwrap_or("unknown"),
        specialized
    );

    PartialAgentState {
        final_response: Some(final_response),
        ..Default::default()
    }
}

fn build_graph() -> CompiledGraph<AgentState> {
    let mut graph = StateGraph::<AgentState>::new();

    let classify_intent = |state: AgentState| async move { NodeResult::Partial(classify_intent(state).await) };
    let code_assist = |state: AgentState| async move { NodeResult::Partial(code_assist(state).await) };
    let qa_assist = |state: AgentState| async move { NodeResult::Partial(qa_assist(state).await) };
    let general_assist = |state: AgentState| async move { NodeResult::Partial(general_assist(state).await) };
    let synthesizer = |state: AgentState| async move { NodeResult::Partial(synthesizer(state).await) };

    graph.add_node("classify_intent", classify_intent);
    graph.add_node("code_assist", code_assist);
    graph.add_node("qa_assist", qa_assist);
    graph.add_node("general_assist", general_assist);
    graph.add_node("synthesizer", synthesizer);

    graph.add_edge("code_assist", "synthesizer");
    graph.add_edge("qa_assist", "synthesizer");
    graph.add_edge("general_assist", "synthesizer");

    graph.set_entry_point("classify_intent");
    graph.set_finish_point("synthesizer");

    let route_intent = |state: &AgentState| -> String {
        match state.intent.as_deref() {
            Some("code") => "code_assist".to_string(),
            Some("qa") => "qa_assist".to_string(),
            _ => "general_assist".to_string(),
        }
    };

    graph.add_conditional_edges(
        "classify_intent",
        route_intent,
        [
            ("code_assist".to_string(), "code_assist".to_string()),
            ("qa_assist".to_string(), "qa_assist".to_string()),
            ("general_assist".to_string(), "general_assist".to_string()),
        ]
        .into(),
    );

    graph.compile()
}

#[derive(Debug, Deserialize)]
struct Config {
    endpoint: String,
    api_key: String,
    model: String,
}

impl Config {
    fn from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let stripped: String = content
            .lines()
            .filter(|line| !line.trim().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let config: Config = serde_json::from_str(&stripped)?;
        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_file("../config.jsonc")?;

    println!("=== Gewu-Graph LLM Workflow Example ===\n");
    println!("Using endpoint: {}", config.endpoint);
    println!("Model: {}\n", config.model);

    let graph = build_graph();

    let test_inputs = vec![
        "How do I implement a binary search tree in Rust?",
        "What is the capital of France?",
        "Can you help me debug this code? I'm getting a segmentation fault.",
    ];

    for (i, input) in test_inputs.iter().enumerate() {
        println!("========================================");
        println!("Test {}: {}", i + 1, input);
        println!("========================================\n");

        let initial_state = AgentState {
            provider: Some(config.endpoint.clone()),
            api_key: Some(config.api_key.clone()),
            model: Some(config.model.clone()),
            user_input: input.to_string(),
            ..Default::default()
        };

        match graph.ainvoke(initial_state).await {
            Ok(result) => {
                println!("Final Response:\n{}", result.final_response);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }

        println!("\n");
    }

    println!("========================================");
    println!("Interactive Mode (type 'exit' to quit)");
    println!("========================================\n");

    loop {
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() || input == "exit" {
            break;
        }

        let initial_state = AgentState {
            provider: Some(config.endpoint.clone()),
            api_key: Some(config.api_key.clone()),
            model: Some(config.model.clone()),
            user_input: input.to_string(),
            ..Default::default()
        };

        match graph.ainvoke(initial_state).await {
            Ok(result) => {
                println!("\n{}\n", result.final_response);
            }
            Err(e) => {
                eprintln!("Error: {}\n", e);
            }
        }
    }

    Ok(())
}
