//! 2D agent "office" visualization.
//!
//! - [`model`] — pure data: rooms, the marker representing an agent, and the
//!   reducer that maps `RuntimeEvent`s to room transitions.
//! - [`render`] — turns the model into a text snapshot that can be fed into a
//!   `CodeEditorView`-backed pane (mirrors the `NetworkLogPane` pattern).

pub mod model;
pub mod pane_manager;
pub mod render;
pub mod view;
