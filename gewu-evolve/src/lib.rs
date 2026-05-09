//! GeWu Evolve - Self-critique, evolution, and tool creation for AI agents.
//!
//! This crate provides capabilities for AI agents to:
//! - Analyze and learn from failures (self-critique)
//! - Retry tasks with improved strategies
//! - Create and save new tools
//! - Execute Python code in a sandboxed environment
//! - Multi-agent orchestration with Planner, Reviewer, Executor
//!
//! # Modules
//!
//! - [`critique`] - Self-critique analysis for error understanding
//! - [`sandbox`] - Python code execution sandbox
//! - [`skill_library`] - Tool persistence and dynamic loading
//! - [`tool_builder`] - LLM-based tool generation
//! - [`graph`] - Evolution state machine for iterative improvement
//! - [`planner`] - Planner agent for creating execution plans
//! - [`reviewer`] - Reviewer agent for plan evaluation and critique
//! - [`executor`] - Executor agent for running plans
//! - [`experience_manager`] - Experience storage and recall
//!
//! # Usage
//!
//! ```ignore
//! use gewu_evolve::{SelfCritique, PythonSandbox, SkillLibrary};
//!
//! // Analyze a failure
//! let critique = SelfCritique::analyze(task, error, llm).await?;
//!
//! // Execute Python code safely
//! let result = sandbox.execute(code, input).await?;
//!
//! // Create and save a new tool
//! let skill = ToolBuilder::generate(description, llm).await?;
//! library.save_skill(&skill).await?;
//! ```

pub mod config;
pub mod critique;
pub mod executor;
pub mod experience_manager;
pub mod graph;
pub mod planner;
pub mod reviewer;
pub mod sandbox;
pub mod skill_library;
pub mod tool_builder;

pub use config::Config;
pub use critique::{CritiqueResult, SelfCritique};
pub use executor::{ExecutionResult, Executor, StepResult};
pub use experience_manager::ExperienceManager;
pub use graph::{EvolutionGraph, EvolutionState, Plan, PlanStep};
pub use reviewer::ReviewResult;
pub use planner::Planner;
pub use reviewer::Reviewer;
pub use sandbox::{PythonResult, PythonSandbox};
pub use skill_library::{Skill, SkillImplementation, SkillLibrary, SkillMetadata};
pub use tool_builder::{GeneratedTool, ToolBuilder, ValidationResult};
