//! # Local Agent Runtime
//!
//! A self-contained, provider-agnostic agent runtime for driving LLM tool-use
//! loops locally. Designed to be used within the Warp terminal client but
//! structured for easy extraction into a standalone service later.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │              AgentRuntime                        │
//! │  (drives the LLM → tool → result loop)          │
//! └─────────┬──────────────────────┬────────────────┘
//!           │                      │
//!    ┌──────▼──────┐       ┌───────▼───────┐
//!    │ LLMProvider │       │ ToolExecutor  │
//!    │  (trait)    │       │  (trait)      │
//!    └──────┬──────┘       └───────┬───────┘
//!           │                      │
//!    ┌──────▼──────┐       ┌───────▼───────┐
//!    │   Ollama    │       │  Warp Bridge  │
//!    │  Provider   │       │ (app layer)   │
//!    └─────────────┘       └───────────────┘
//! ```
//!
//! ## Key Types
//!
//! - [`AgentRuntime`] — The core loop engine
//! - [`LLMProvider`] — Trait for LLM backends (Ollama, OpenAI-compat, etc.)
//! - [`ToolExecutor`] — Trait for tool execution (implemented by the app)
//! - [`RuntimeEvent`] — Events yielded by the runtime
//! - [`RuntimeConfig`] — Configuration knobs
//!
//! ## Usage
//!
//! ```rust,ignore
//! use local_agent_runtime::{AgentRuntime, RuntimeConfig};
//! use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
//!
//! let provider = OllamaProvider::new(OllamaProviderConfig::default());
//! let executor = MyToolExecutor::new(); // implements ToolExecutor
//! let config = RuntimeConfig::default();
//!
//! let runtime = AgentRuntime::new(provider, executor, config);
//! let (events, history) = runtime
//!     .run_to_completion("qwen2.5-coder:7b", vec![], "List files in /tmp")
//!     .await?;
//! ```

pub mod config;
pub mod error;
pub mod events;
pub mod messages;
pub mod provider;
pub mod runtime;
pub mod tools;

// Re-export primary types at crate root for convenience.
pub use config::RuntimeConfig;
pub use error::{ProviderError, RuntimeError, ToolExecutionError};
pub use events::{FinishReason, RunResult, RuntimeEvent, StopReason};
pub use messages::Message;
pub use provider::{
    ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent, LLMProvider, ProviderCapabilities,
};
pub use runtime::{AgentRuntime, CancelHandle};
pub use tools::schema::{ToolSchema, ToolSchemaBuilder};
pub use tools::{PermissionDecision, ToolCall, ToolCallResult, ToolExecutor, ToolSafetyClass};
