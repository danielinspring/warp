//! 2D agent "office" visualization.
//!
//! - [`event`] — the feed the pane folds, derived from any agent run's response stream.
//! - [`model`] — pure data: rooms, the marker representing an agent, and the reducer that maps
//!   events to room transitions.
//! - [`spec`] — the prompt and tool list, read from the local agent service.
//! - [`render`] — turns the model into a text snapshot that can be fed into a
//!   `CodeEditorView`-backed pane (mirrors the `NetworkLogPane` pattern).

pub mod context;
pub mod event;
pub mod model;
pub mod pane_manager;
pub mod render;
pub mod spec;
pub mod view;
