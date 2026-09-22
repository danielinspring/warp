//! The visualization's own event feed.
//!
//! The pane used to fold the local runtime's own events, which only existed while the agent ran
//! inside the app. It now folds a small event derived from the response stream every agent run
//! produces, so the pane works for a local service run and a cloud run alike.
//!
//! Publishers have no `AppContext`, so the feed is a process-wide broadcast channel that any UI
//! surface subscribes to.

use std::sync::OnceLock;

use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 256;

/// What the visualization needs to know about a run, distilled from the response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentVizEvent {
    /// A request started.
    TurnStarted,
    /// The agent asked for a tool, named by the client action it produced.
    ToolRequested { tool_name: String },
    /// A tool began executing.
    ToolStarted { tool_name: String },
    /// A tool finished, successfully or not.
    ToolFinished,
    /// A tool is blocked on the user's confirmation.
    PermissionRequired,
    /// The agent produced text, with a short preview for the status line.
    Text { preview: String },
    /// The run ended, carrying the error that ended it when there was one.
    Finished { error: Option<String> },
}

#[derive(Debug, Clone)]
pub struct RunScopedEvent {
    pub run_id: String,
    pub event: AgentVizEvent,
}

static SENDER: OnceLock<broadcast::Sender<RunScopedEvent>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<RunScopedEvent> {
    SENDER.get_or_init(|| broadcast::channel(CHANNEL_CAPACITY).0)
}

/// Publish an event to all subscribers. A no-op when nobody is listening, which is the common
/// case since the pane is usually closed.
///
/// An empty `run_id` means "whatever run is current", which is all a publisher outside the
/// response stream can say.
pub fn publish(run_id: &str, event: AgentVizEvent) {
    let _ = sender().send(RunScopedEvent {
        run_id: run_id.to_string(),
        event,
    });
}

/// Subscribe to all subsequent events. The receiver sees only what is published after this call.
pub fn subscribe() -> broadcast::Receiver<RunScopedEvent> {
    sender().subscribe()
}

/// Subscribe and forward into an `async_channel::Receiver` so warpui's `spawn_stream_local` can
/// drive view updates from the UI thread.
///
/// The pump runs on a dedicated std thread rather than a Tokio task because callers like
/// `AgentVizView::new` execute on the UI thread, which has no reactor in scope. The thread exits
/// when the receiver, and therefore the view owning it, is dropped.
pub fn subscribe_local() -> async_channel::Receiver<RunScopedEvent> {
    let (tx, rx) = async_channel::unbounded::<RunScopedEvent>();
    let mut bcast_rx = subscribe();
    std::thread::spawn(move || {
        loop {
            match bcast_rx.blocking_recv() {
                Ok(scoped) => {
                    // The lint steers callers to `block_on` because `send_blocking` is missing on
                    // wasm. This pump is a dedicated std thread in a desktop-only pane, and it has
                    // no reactor to block on.
                    #[allow(clippy::disallowed_methods)]
                    let delivered = tx.send_blocking(scoped);
                    if delivered.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    rx
}

/// Announce that a client tool started, naming it from the action the client is about to run.
///
/// The action model is the only place that knows a tool actually began, as opposed to having been
/// asked for, which is why publishing happens there rather than from the response stream.
pub fn publish_tool_started(action: Option<&crate::ai::agent::AIAgentAction>) {
    let tool_name = action
        .map(|action| {
            let rendered = format!("{:?}", action.action);
            rendered
                .split(['(', ' ', '{'])
                .next()
                .unwrap_or("tool")
                .to_string()
        })
        .unwrap_or_else(|| "tool".to_string());
    publish("", AgentVizEvent::ToolStarted { tool_name });
}
