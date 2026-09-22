//! Standalone local agent service.
//!
//! Speaks Warp's multi-agent protobuf over HTTP+SSE and drives `local_agent_runtime` against the
//! OpenAI-compatible provider forwarded in each request. Web, git, todo and skill-catalog tools run
//! in-process; every other tool call is handed back to the Warp client for execution.

pub mod event_mapper;
pub mod executor;
pub mod git;
pub mod git_helpers;
pub mod images;
pub mod model_packs;
pub mod prompt;
pub mod provider;
pub mod registry;
pub mod request;
pub mod server;
pub mod todos;
pub mod tool_proto;
pub mod tool_results;
pub mod turn;
pub mod web;

#[cfg(test)]
pub(crate) mod test_support;

pub use provider::{OllamaProviderFactory, ProviderConfig, ProviderFactory};
pub use server::{ServerState, router, serve};
