use anyhow::Result;
use gewu_graph::{CompiledGraph, GraphTool, NodeResult, State, StateGraph};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ResearchState {
    query: String,
    search_queries: Vec<String>,
    search_results: Vec<String>,
    synthesized_report: String,
    final_response: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PartialResearchState {
    query: Option<String>,
    search_queries: Option<Vec<String>>,
    search_results: Option<Vec<String>>,
    synthesized_report: Option<String>,
    final_response: Option<String>,
}

impl State for ResearchState {
    type Partial = PartialResearchState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            query: update.query.unwrap_or_else(|| self.query.clone()),
            search_queries: update
                .search_queries
                .unwrap_or_else(|| self.search_queries.clone()),
            search_results: update
                .search_results
                .unwrap_or_else(|| self.search_results.clone()),
            synthesized_report: update
                .synthesized_report
                .unwrap_or_else(|| self.synthesized_report.clone()),
            final_response: update
                .final_response
                .unwrap_or_else(|| self.final_response.clone()),
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

async fn generate_search_queries(
    state: ResearchState,
    provider: &str,
    api_key: &str,
    model: &str,
) -> PartialResearchState {
    let system = r#"Given a research query, generate exactly 2 search queries that would help gather information on this topic.
Output ONLY a JSON array with 2 strings. Example: ["query 1", "query 2"]"#;

    let queries = match call_llm(provider, api_key, model, system, &state.query).await {
        Ok(q) => q,
        Err(_) => {
            return PartialResearchState {
                query: Some(state.query),
                ..Default::default()
            };
        }
    };

    let search_queries: Vec<String> = serde_json::from_str(&queries)
        .unwrap_or_else(|_| vec![state.query.clone(), state.query.clone()]);

    PartialResearchState {
        query: Some(state.query),
        search_queries: Some(search_queries),
        ..Default::default()
    }
}

async fn perform_searches(
    state: ResearchState,
    provider: &str,
    api_key: &str,
    model: &str,
) -> PartialResearchState {
    let queries = state.search_queries.clone();
    let mut results = Vec::new();

    for query in queries {
        let system = r#"You are a search engine. Given a query, provide a comprehensive response
as if you were searching the web. Include relevant facts, opinions, and details."#;

        let result = match call_llm(provider, api_key, model, system, &query).await {
            Ok(r) => r,
            Err(_) => format!("Search result for: {}", query),
        };
        results.push(result);
    }

    PartialResearchState {
        search_results: Some(results),
        ..Default::default()
    }
}

async fn synthesize(
    state: ResearchState,
    provider: &str,
    api_key: &str,
    model: &str,
) -> PartialResearchState {
    let results_text = state
        .search_results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Search Result {}:\n{}\n", i + 1, r))
        .collect::<Vec<_>>()
        .join("\n---\n");

    let system = r#"You are a research synthesizer. Given a research query and several search results,
create a well-structured, comprehensive report that synthesizes all the information.
Format with clear sections and key findings."#;

    let user = format!(
        "Research Query: {}\n\nSearch Results:\n{}",
        state.query, results_text
    );

    let report = match call_llm(provider, api_key, model, system, &user).await {
        Ok(r) => r,
        Err(_) => format!("Synthesized report on: {}", state.query),
    };

    PartialResearchState {
        synthesized_report: Some(report),
        ..Default::default()
    }
}

async fn format_response(
    state: ResearchState,
    provider: &str,
    api_key: &str,
    model: &str,
) -> PartialResearchState {
    let system = r#"You are a research assistant presenting findings to a user.
Given a research query and a synthesized report, format it into a clear, engaging response
that addresses the original query. Use bullet points, headings, and keep it accessible."#;

    let user = format!(
        "Original Query: {}\n\nReport:\n{}",
        state.query, state.synthesized_report
    );

    let response = match call_llm(provider, api_key, model, system, &user).await {
        Ok(r) => r,
        Err(_) => state.synthesized_report.clone(),
    };

    PartialResearchState {
        final_response: Some(response),
        ..Default::default()
    }
}

fn build_research_graph(
    endpoint: &str,
    api_key: &str,
    model: &str,
) -> CompiledGraph<ResearchState> {
    let mut graph = StateGraph::<ResearchState>::new();

    let p1 = endpoint.to_string();
    let k1 = api_key.to_string();
    let m1 = model.to_string();
    let generate_queries = move |state: ResearchState| {
        let p = p1.clone();
        let k = k1.clone();
        let m = m1.clone();
        async move { NodeResult::Partial(generate_search_queries(state, &p, &k, &m).await) }
    };

    let p2 = endpoint.to_string();
    let k2 = api_key.to_string();
    let m2 = model.to_string();
    let perform_searches = move |state: ResearchState| {
        let p = p2.clone();
        let k = k2.clone();
        let m = m2.clone();
        async move { NodeResult::Partial(perform_searches(state, &p, &k, &m).await) }
    };

    let p3 = endpoint.to_string();
    let k3 = api_key.to_string();
    let m3 = model.to_string();
    let synthesize = move |state: ResearchState| {
        let p = p3.clone();
        let k = k3.clone();
        let m = m3.clone();
        async move { NodeResult::Partial(synthesize(state, &p, &k, &m).await) }
    };

    let p4 = endpoint.to_string();
    let k4 = api_key.to_string();
    let m4 = model.to_string();
    let format_response = move |state: ResearchState| {
        let p = p4.clone();
        let k = k4.clone();
        let m = m4.clone();
        async move { NodeResult::Partial(format_response(state, &p, &k, &m).await) }
    };

    graph.add_node("generate_queries", generate_queries);
    graph.add_node("search", perform_searches);
    graph.add_node("synthesize", synthesize);
    graph.add_node("respond", format_response);

    graph
        .add_edge("generate_queries", "search")
        .expect("valid edge");
    graph.add_edge("search", "synthesize").expect("valid edge");
    graph.add_edge("synthesize", "respond").expect("valid edge");

    graph.set_entry_point("generate_queries");
    graph.set_finish_point("respond");

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
    println!("=== Gewu-Graph Research Agent Example ===\n");

    let config = Config::from_file("../config.jsonc")?;
    println!("Using endpoint: {}", config.endpoint);
    println!("Model: {}\n", config.model);

    let research_graph = build_research_graph(&config.endpoint, &config.api_key, &config.model);
    let research_tool = GraphTool::new(
        "research",
        "Conducts multi-step research: generates search queries, performs searches, synthesizes findings, and formats a response",
        research_graph.clone(),
    );

    println!("Research tool created: {}\n", research_tool.name());
    println!("Running research queries...\n");
    let research_tool = GraphTool::new(
        "research",
        "Conducts multi-step research: generates search queries, performs searches, synthesizes findings, and formats a response",
        research_graph.clone(),
    );

    println!("Research tool created: {}\n", research_tool.tool_name());
    println!("Running research queries...\n");

    let test_queries = ["What are the benefits of Rust for systems programming?"];

    for (i, query) in test_queries.iter().enumerate() {
        println!("========================================");
        println!("Research Query {}: {}", i + 1, query);
        println!("========================================\n");

        let initial_state = ResearchState {
            query: query.to_string(),
            ..Default::default()
        };

        let result = research_graph.ainvoke(initial_state).await;

        match result {
            Ok(state) => {
                println!("Final Response:\n{}", state.final_response);
            }
            Err(e) => {
                eprintln!("Error during research: {}", e);
            }
        }

        println!("\n");
    }

    Ok(())
}
