//! Process-wide broadcast bus for `RuntimeEvent`s coming out of the local
//! agent runtime.
//!
//! The runtime loop in [`super::local_runtime_integration`] runs inside a
//! `tokio::spawn` and therefore has no `AppContext`. Rather than threading a
//! context through, we expose a `OnceCell<broadcast::Sender>` that the loop
//! publishes to, and any UI surface (the agent visualization, future panels)
//! subscribes via [`subscribe`].
//!
//! `RuntimeEvent` derives `Clone`, so broadcast fan-out is cheap.

use std::sync::OnceLock;

use local_agent_runtime::RuntimeEvent;
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct RunScopedEvent {
    pub run_id: String,
    pub event: RuntimeEvent,
}

static SENDER: OnceLock<broadcast::Sender<RunScopedEvent>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<RunScopedEvent> {
    SENDER.get_or_init(|| broadcast::channel(CHANNEL_CAPACITY).0)
}

/// Publish a runtime event to all subscribers. Silently no-ops if there are
/// no subscribers — matches `broadcast::Sender::send` semantics.
pub fn publish(run_id: &str, event: RuntimeEvent) {
    let _ = sender().send(RunScopedEvent {
        run_id: run_id.to_string(),
        event,
    });
}

/// Subscribe to all subsequent runtime events. The receiver will see only
/// events published after this call.
pub fn subscribe() -> broadcast::Receiver<RunScopedEvent> {
    sender().subscribe()
}

/// Subscribe and forward into an `async_channel::Receiver` so warpui's
/// `spawn_stream_local` can drive view updates from the UI thread.
///
/// The pump runs on a dedicated std thread (not a Tokio task) because callers
/// like `AgentVizView::new` execute on the UI thread, which has no Tokio
/// reactor in scope. The thread exits when the async-channel sender is
/// dropped — i.e. when the view that owns the receiver is dropped.
pub fn subscribe_local() -> async_channel::Receiver<RunScopedEvent> {
    let (tx, rx) = async_channel::unbounded::<RunScopedEvent>();
    let mut bcast_rx = subscribe();
    std::thread::spawn(move || loop {
        match bcast_rx.blocking_recv() {
            Ok(scoped) => {
                if tx.send_blocking(scoped).is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    });
    rx
}
