use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::secret::ShareSecret;
use super::server;

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
}

/// Shared, mutable state read by the axum handlers on every request. Kept
/// separate from [`LocalSessionShareHub`] so it can be cheaply cloned into
/// the router without exposing hub lifecycle methods to request handlers.
pub(crate) struct ShareState {
    secret: RwLock<Option<ShareSecret>>,
}

impl ShareState {
    fn new(secret: ShareSecret) -> Self {
        Self {
            secret: RwLock::new(Some(secret)),
        }
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
    pub fn start(&mut self, bind_ip: IpAddr, port: u16) -> Result<ShareHandle, HubError> {
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

        let secret = ShareSecret::generate();
        let state = Arc::new(ShareState::new(secret.clone()));
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

#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
