use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use session_sharing_protocol::common::{OrderedTerminalEventType, Scrollback, WindowSize};
use session_sharing_protocol::viewer::DownstreamMessage;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};

use super::protocol::{compress_pty_bytes, ordered_event_downstream};
use super::secret::ShareSecret;
use super::server;

/// Environment variable consulted when [`LocalSessionShareHub::start`] is
/// called without an explicit WASM bundle directory.
pub const WASM_BUNDLE_DIR_ENV: &str = "WARP_LOCAL_SHARE_WASM_DIR";

/// Maximum scrollback snapshot size served to local-share guests on join
/// (PRODUCT.md P20). Older blocks are dropped from the front when capping.
pub const LOCAL_SHARE_MAX_SCROLLBACK_BYTES: u64 = 10 * 1024 * 1024;

const EVENT_BROADCAST_CAPACITY: usize = 256;

/// A live local session share, returned by [`LocalSessionShareHub::start`]
/// and [`LocalSessionShareHub::rotate_secret`]. Holding onto this is not
/// required to keep the share alive; the hub owns the lifetime.
#[derive(Debug, Clone)]
pub struct ShareHandle {
    /// The full share URL, including the secret, for example
    /// `http://192.168.1.23:51234/local-session/<secret>` (PRODUCT.md P5).
    pub url: String,
    pub secret: ShareSecret,
    pub addr: SocketAddr,
}

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("a local session share is already active on this hub")]
    AlreadyActive,
    #[error("no local session share is active on this hub")]
    NotActive,
    #[error("failed to bind local session share listener on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start local session share runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to serialize session-sharing-protocol message: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// Shared, mutable state read by the axum handlers on every request. Kept
/// separate from [`LocalSessionShareHub`] so it can be cheaply cloned into
/// the router without exposing hub lifecycle methods to request handlers.
pub(crate) struct ShareState {
    secret: RwLock<Option<ShareSecret>>,
    window_size: RwLock<WindowSize>,
    scrollback: RwLock<Scrollback>,
    next_event_no: AtomicUsize,
    event_tx: broadcast::Sender<String>,
    /// Optional directory containing `index.html`, `wasm/`, and `assets/` for
    /// serving the Warp WASM viewer over the share URL.
    wasm_bundle_dir: Option<PathBuf>,
}

impl ShareState {
    fn new(secret: ShareSecret, wasm_bundle_dir: Option<PathBuf>) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self {
            secret: RwLock::new(Some(secret)),
            window_size: RwLock::new(WindowSize::default()),
            scrollback: RwLock::new(Scrollback {
                blocks: vec![],
                is_alt_screen_active: false,
            }),
            next_event_no: AtomicUsize::new(0),
            event_tx,
            wasm_bundle_dir,
        }
    }

    pub(crate) fn wasm_bundle_dir(&self) -> Option<&std::path::Path> {
        self.wasm_bundle_dir.as_deref()
    }

    pub(crate) fn check_secret(&self, candidate: &str) -> bool {
        self.secret
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|secret| secret.matches(candidate))
    }

    fn set_secret(&self, secret: ShareSecret) {
        *self
            .secret
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(secret);
    }

    fn invalidate(&self) {
        *self
            .secret
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(crate) fn window_size(&self) -> WindowSize {
        *self
            .window_size
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_window_size(&self, size: WindowSize) {
        *self
            .window_size
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = size;
    }

    pub(crate) fn scrollback(&self) -> Scrollback {
        self.scrollback
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_scrollback(&self, scrollback: Scrollback) {
        *self
            .scrollback
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = scrollback;
    }

    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    fn publish_downstream(&self, message: DownstreamMessage) -> Result<(), HubError> {
        let json = message.to_json().map_err(HubError::Serialize)?;
        // No active subscribers is fine — host may publish before guests join.
        let _ = self.event_tx.send(json);
        Ok(())
    }

    fn publish_event_type(&self, event_type: OrderedTerminalEventType) -> Result<(), HubError> {
        let event_no = self.next_event_no.fetch_add(1, Ordering::SeqCst);
        self.publish_downstream(ordered_event_downstream(event_no, event_type))
    }
}

/// Cloneable handle for publishing host PTY/events into an active local share
/// without holding the [`LocalSessionShareHub`] lock on the TerminalModel path.
#[derive(Clone)]
pub struct LocalShareEventPublisher {
    state: Arc<ShareState>,
}

impl LocalShareEventPublisher {
    /// Publishes raw host PTY output to connected guests (LZ4 size-prepended).
    pub fn publish_pty_bytes(&self, bytes: &[u8]) -> Result<(), HubError> {
        let compressed = compress_pty_bytes(bytes);
        self.state
            .publish_event_type(OrderedTerminalEventType::PtyBytesRead { bytes: compressed })
    }

    pub fn publish_event(&self, event_type: OrderedTerminalEventType) -> Result<(), HubError> {
        self.state.publish_event_type(event_type)
    }

    pub fn set_window_size(&self, size: WindowSize) {
        self.state.set_window_size(size);
    }
}

/// The currently running share for a [`LocalSessionShareHub`], including the
/// private tokio runtime that drives its axum server. Dropping (or explicitly
/// tearing down) this struct stops accepting new connections.
struct ActiveShare {
    state: Arc<ShareState>,
    addr: SocketAddr,
    secret: ShareSecret,
    /// The tokio runtime driving the axum server for this share. We use a
    /// private runtime per hub, mirroring `crates/http_server`, because the
    /// local session share hub must be independently start/stoppable and is
    /// not tied to the loopback-only `HttpServer` singleton.
    runtime: tokio::runtime::Runtime,
    shutdown: oneshot::Sender<()>,
}

/// Owns at most one active local network share (PRODUCT.md P3). A terminal
/// pane will own one `LocalSessionShareHub` instance in a follow-up PR; this
/// hub only knows about binding, secrets, and HTTP/WS serving, not panes.
#[derive(Default)]
pub struct LocalSessionShareHub {
    active: Option<ActiveShare>,
}

impl LocalSessionShareHub {
    pub fn new() -> Self {
        Self { active: None }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Returns the handle for the currently active share, if any, without
    /// rotating the secret. Lets the host re-copy the current link
    /// (PRODUCT.md P9).
    pub fn current_handle(&self) -> Option<ShareHandle> {
        self.active.as_ref().map(|active| ShareHandle {
            url: build_url(active.addr, &active.secret),
            secret: active.secret.clone(),
            addr: active.addr,
        })
    }

    /// Binds a new local session share on `bind_ip:port` (use port `0` for
    /// an OS-assigned ephemeral port) and starts serving it. Fails if a
    /// share is already active on this hub (PRODUCT.md P3) or if the address
    /// cannot be bound (PRODUCT.md P8).
    ///
    /// When `wasm_bundle_dir` is `None`, falls back to
    /// [`WASM_BUNDLE_DIR_ENV`] if set.
    pub fn start(&mut self, bind_ip: IpAddr, port: u16) -> Result<ShareHandle, HubError> {
        self.start_with_options(bind_ip, port, None)
    }

    /// Like [`start`](Self::start), but accepts an explicit WASM bundle
    /// directory. When `wasm_bundle_dir` is `None`, falls back to
    /// [`WASM_BUNDLE_DIR_ENV`].
    pub fn start_with_options(
        &mut self,
        bind_ip: IpAddr,
        port: u16,
        wasm_bundle_dir: Option<PathBuf>,
    ) -> Result<ShareHandle, HubError> {
        if self.active.is_some() {
            return Err(HubError::AlreadyActive);
        }

        let requested_addr = SocketAddr::new(bind_ip, port);
        let std_listener =
            std::net::TcpListener::bind(requested_addr).map_err(|source| HubError::Bind {
                addr: requested_addr,
                source,
            })?;
        std_listener
            .set_nonblocking(true)
            .map_err(|source| HubError::Bind {
                addr: requested_addr,
                source,
            })?;
        let addr = std_listener.local_addr().map_err(|source| HubError::Bind {
            addr: requested_addr,
            source,
        })?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(HubError::Runtime)?;

        let wasm_bundle_dir = resolve_wasm_bundle_dir(wasm_bundle_dir);
        let secret = ShareSecret::generate();
        let state = Arc::new(ShareState::new(secret.clone(), wasm_bundle_dir));
        let router = server::build_router(state.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        runtime.spawn(async move {
            let listener = match TcpListener::from_std(std_listener) {
                Ok(listener) => listener,
                Err(err) => {
                    log::error!("Failed to start local session share listener: {err}");
                    return;
                }
            };

            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(err) = result {
                log::error!("Local session share server exited with error: {err}");
            }
        });

        let handle = ShareHandle {
            url: build_url(addr, &secret),
            secret: secret.clone(),
            addr,
        };

        self.active = Some(ActiveShare {
            state,
            addr,
            secret,
            runtime,
            shutdown: shutdown_tx,
        });

        Ok(handle)
    }

    /// Immediately invalidates the current secret and tears down the server,
    /// closing guest connections (PRODUCT.md P10, P11).
    pub fn stop(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        // Invalidate first so any in-flight request racing the shutdown
        // still gets rejected rather than served.
        active.state.invalidate();
        let _ = active.shutdown.send(());
        // `shutdown_background` returns immediately and cancels outstanding
        // tasks in the background, so `stop()` never blocks the caller.
        active.runtime.shutdown_background();
    }

    /// Invalidates the current secret and issues a new one, keeping the same
    /// bind address (PRODUCT.md P12). Guests on the old URL lose access and
    /// are not silently migrated to the new secret.
    pub fn rotate_secret(&mut self) -> Result<ShareHandle, HubError> {
        let active = self.active.as_mut().ok_or(HubError::NotActive)?;
        let new_secret = ShareSecret::generate();
        active.state.set_secret(new_secret.clone());
        active.secret = new_secret.clone();

        Ok(ShareHandle {
            url: build_url(active.addr, &new_secret),
            secret: new_secret,
            addr: active.addr,
        })
    }

    /// Updates the window size advertised to guests on join and used for
    /// subsequent Resize events the host may publish.
    pub fn set_window_size(&self, size: WindowSize) -> Result<(), HubError> {
        let active = self.active.as_ref().ok_or(HubError::NotActive)?;
        active.state.set_window_size(size);
        Ok(())
    }

    /// Sets the scrollback snapshot served to guests on
    /// [`DownstreamMessage::JoinedSuccessfully`]. Oversized snapshots are
    /// capped by dropping oldest blocks until under
    /// [`LOCAL_SHARE_MAX_SCROLLBACK_BYTES`] (PRODUCT.md P20).
    pub fn set_scrollback(&self, mut scrollback: Scrollback) -> Result<(), HubError> {
        let active = self.active.as_ref().ok_or(HubError::NotActive)?;
        cap_scrollback(&mut scrollback, LOCAL_SHARE_MAX_SCROLLBACK_BYTES);
        active.state.set_scrollback(scrollback);
        Ok(())
    }

    /// Returns a cloneable publisher for the active share, if any. Used by
    /// [`TerminalModel`] to fan PTY bytes into the hub without owning the hub.
    pub fn event_publisher(&self) -> Option<LocalShareEventPublisher> {
        self.active.as_ref().map(|active| LocalShareEventPublisher {
            state: active.state.clone(),
        })
    }

    /// Publishes raw host PTY output to connected guests. Bytes are LZ4
    /// size-prepended to match the cloud sharer path.
    pub fn publish_pty_bytes(&self, bytes: &[u8]) -> Result<(), HubError> {
        let publisher = self.event_publisher().ok_or(HubError::NotActive)?;
        publisher.publish_pty_bytes(bytes)
    }

    /// Publishes an ordered terminal event to connected guests.
    pub fn publish_event(&self, event_type: OrderedTerminalEventType) -> Result<(), HubError> {
        let publisher = self.event_publisher().ok_or(HubError::NotActive)?;
        publisher.publish_event(event_type)
    }
}

impl Drop for LocalSessionShareHub {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_url(addr: SocketAddr, secret: &ShareSecret) -> String {
    let host = match addr.ip() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{}/local-session/{secret}", addr.port())
}

fn resolve_wasm_bundle_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| std::env::var_os(WASM_BUNDLE_DIR_ENV).map(PathBuf::from))
}

/// Drops oldest scrollback blocks until `scrollback` fits under `max_bytes`.
pub(crate) fn cap_scrollback(scrollback: &mut Scrollback, max_bytes: u64) {
    while scrollback.num_bytes().as_u64() > max_bytes && !scrollback.blocks.is_empty() {
        scrollback.blocks.remove(0);
    }
}

#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
