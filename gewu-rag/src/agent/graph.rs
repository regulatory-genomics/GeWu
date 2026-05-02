use crate::agent::edges;
use crate::agent::nodes;
use crate::agent::state::{AgentState, MainState, PartialAgentState, PartialMainState};
use crate::config::Config;
use crate::retrieval::parent_store::ParentStore;
use crate::retrieval::qdrant_store::QdrantStore;
use crate::tools::retrieval::{
    RetrieveManyParentChunksTool, RetrieveParentChunksTool, SearchChildChunksTool,
};
use gewu_core::types::Message;
use gewu_core::{LlmProvider, Tool};
use gewu_graph::{CompiledGraph, NodeResult, StateGraph};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static GRAPH_NODE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_graph_node_id() -> usize {
    GRAPH_NODE_COUNTER.fetch_add(1, Ordering::SeqCst)
}

pub struct RagSystem {
    config: Config,
    llm: Arc<dyn LlmProvider>,
    vector_store: Arc<QdrantStore>,
    parent_store: Arc<ParentStore>,
}

impl RagSystem {
    pub async fn new(
        config: Config,
        llm: Arc<dyn LlmProvider>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let vector_store = Arc::new(QdrantStore::new(config.clone()).await?);
        let parent_store = Arc::new(ParentStore::new(config.parent_store_path.clone()));

        Ok(Self {
            config,
            llm,
            vector_store,
            parent_store,
        })
    }

    pub fn get_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(SearchChildChunksTool::new(
                self.vector_store.clone(),
                self.parent_store.clone(),
                self.config.clone(),
            )),
            Arc::new(RetrieveParentChunksTool::new(self.parent_store.clone())),
            Arc::new(RetrieveManyParentChunksTool::new(self.parent_store.clone())),
        ]
    }

    pub async fn query(
        &self,
        question: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let messages = vec![Message::User {
            content: vec![gewu_core::types::ContentPart::Text {
                text: question.to_string(),
            }],
        }];

        let initial_state = MainState {
            messages,
            ..Default::default()
        };

        let agent_graph = self.build_agent_graph();
        let main_graph = self.build_main_graph(agent_graph);

        let final_state = main_graph.ainvoke(initial_state).await?;

        if let Some(last_msg) = final_state.messages.last() {
            if let Message::Assistant { content, .. } = last_msg {
                Ok(content
                    .clone()
                    .unwrap_or_else(|| "No response generated.".to_string()))
            } else {
                Ok("No response generated.".to_string())
            }
        } else {
            Ok("No response generated.".to_string())
        }
    }

    fn build_agent_graph(&self) -> CompiledGraph<AgentState> {
        let config = self.config.clone();
        let llm = self.llm.clone();
        let tools = self.get_tools();

        let mut graph = StateGraph::<AgentState>::new();

        graph.add_node("orchestrator", move |state: AgentState| {
            let llm = llm.clone();
            let tools = tools.clone();
            let config = config.clone();

            async move {
                NodeResult::Partial(nodes::orchestrator(state, llm, tools, &config).await)
            }
        });

        let tools2 = self.get_tools();
        graph.add_node("tools", move |state: AgentState| {
            let tools = tools2.clone();

            async move { NodeResult::Partial(nodes::execute_tools(state, tools).await) }
        });

        graph.add_node("should_compress", |state: AgentState| async move {
            let node_id = next_graph_node_id();
            tracing::info!(
                "[GRAPH_NODE #{}] should_compress: tool_count={}, iteration={}, msgs={}",
                node_id,
                state.tool_call_count,
                state.iteration_count,
                state.messages.len()
            );
            NodeResult::Partial(PartialAgentState {
                messages: Some(state.messages.clone()),
                ..Default::default()
            })
        });

        let llm2 = self.llm.clone();
        graph.add_node("compress_context", move |state: AgentState| {
            let llm = llm2.clone();

            async move { NodeResult::Partial(nodes::compress_context(state, llm).await) }
        });

        let llm3 = self.llm.clone();
        graph.add_node("fallback_response", move |state: AgentState| {
            let llm = llm3.clone();

            async move { NodeResult::Partial(nodes::fallback_response(state, llm).await) }
        });

        graph.add_node("collect_answer", move |state: AgentState| async move {
            NodeResult::Partial(nodes::collect_answer(state).await)
        });

        graph.set_entry_point("orchestrator");
        graph.set_finish_point("collect_answer");

        let config3 = self.config.clone();
        graph.add_conditional_edges(
            "orchestrator",
            move |state: &AgentState| edges::route_after_orchestrator(state, &config3.clone()),
            {
                let mut map = HashMap::new();
                map.insert("tools".to_string(), "tools".to_string());
                map.insert(
                    "fallback_response".to_string(),
                    "fallback_response".to_string(),
                );
                map.insert("collect_answer".to_string(), "collect_answer".to_string());
                map
            },
        );

        let config4 = self.config.clone();
        graph.add_conditional_edges(
            "should_compress",
            move |state: &AgentState| edges::route_after_tools(state, &config4.clone()),
            {
                let mut map = HashMap::new();
                map.insert(
                    "compress_context".to_string(),
                    "compress_context".to_string(),
                );
                map.insert("orchestrator".to_string(), "orchestrator".to_string());
                map.insert("collect_answer".to_string(), "collect_answer".to_string());
                map
            },
        );

        graph.add_edge("tools", "should_compress");
        graph.add_edge("compress_context", "orchestrator");
        graph.add_edge("fallback_response", "collect_answer");

        graph.compile()
    }

    fn build_main_graph(
        &self,
        agent_subgraph: CompiledGraph<AgentState>,
    ) -> CompiledGraph<MainState> {
        let llm = self.llm.clone();
        let agent_subgraph = agent_subgraph.clone();

        let mut graph = StateGraph::<MainState>::new();

        graph.add_node("summarize_history", move |state: MainState| {
            let llm = llm.clone();

            async move { NodeResult::Partial(nodes::summarize_history(state, llm).await) }
        });

        let llm2 = self.llm.clone();
        graph.add_node("rewrite_query", move |state: MainState| {
            let llm = llm2.clone();

            async move { NodeResult::Partial(nodes::rewrite_query(state, llm).await) }
        });

        graph.add_node("request_clarification", move |state: MainState| {
            let clarification = state.clarification_needed.clone().unwrap_or_else(|| {
                "I need more information to understand your question.".to_string()
            });

            let mut messages = state.messages.clone();
            messages.push(Message::Assistant {
                content: Some(clarification),
                tool_calls: None,
            });

            async move {
                NodeResult::Partial(PartialMainState {
                    messages: Some(messages),
                    ..Default::default()
                })
            }
        });

        let llm3 = self.llm.clone();
        graph.add_node("run_agents", move |state: MainState| {
            let llm = llm3.clone();
            let subgraph = agent_subgraph.clone();

            async move {
                NodeResult::Partial(nodes::run_parallel_agents(state, llm, subgraph).await)
            }
        });

        let llm4 = self.llm.clone();
        graph.add_node("aggregate_answers", move |state: MainState| {
            let llm = llm4.clone();

            async move { NodeResult::Partial(nodes::aggregate_answers(state, llm).await) }
        });

        graph.set_entry_point("summarize_history");
        graph.add_edge("summarize_history", "rewrite_query");

        graph.add_conditional_edges(
            "rewrite_query",
            |state: &MainState| edges::route_after_rewrite(state),
            {
                let mut map = HashMap::new();
                map.insert(
                    "request_clarification".to_string(),
                    "request_clarification".to_string(),
                );
                map.insert("run_agents".to_string(), "run_agents".to_string());
                map
            },
        );

        graph.add_edge("request_clarification", "rewrite_query");
        graph.add_edge("run_agents", "aggregate_answers");
        graph.set_finish_point("aggregate_answers");

        graph.compile()
    }
}
