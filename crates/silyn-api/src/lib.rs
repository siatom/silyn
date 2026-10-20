//! Local HTTP and SSE surface for the Silyn application service.
//!
//! This crate is a transport adapter, not a second service layer: validation,
//! run wiring, memory scoping and the evolution gates all live in
//! [`silyn_app`]. The server is single-tenant, binds to loopback by default and
//! exposes no capability the CLI does not already have.
//!
//! - [`router`] builds the `axum` router for an [`ApiState`] (used by tests).
//! - [`serve`] resolves the bind flag from the environment and runs the server.
//! - [`ApiConfig`] is the environment-backed configuration; [`serve_with`] takes
//!   the same values explicitly.
//!
//! Route table and error mapping are documented in `docs/gates/w7-01.md`.

mod auth;
mod body;
mod error;
mod evolution;
mod routes;

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use silyn_app::App;
use silyn_core::SilynError;

pub use error::ApiError;
/// Version reported by `GET /health` and by [`silyn_app::version_line`].
pub use silyn_app::VERSION;

/// Environment variable holding the optional bearer token.
pub const TOKEN_ENV: &str = auth::TOKEN_ENV;
/// Environment variable holding the bind address.
pub const ADDR_ENV: &str = "SILYN_API_ADDR";
/// Environment variable that permits a non-loopback bind (`1`, `true` or `yes`).
pub const ALLOW_NON_LOOPBACK_ENV: &str = "SILYN_API_ALLOW_NON_LOOPBACK";
/// Default bind address. Loopback only.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7788";
/// How often `GET /v1/runs/{id}/events` re-reads the journal.
pub const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Capacity of the channel between the poll task and the SSE response.
///
/// A slow client blocks the poll task instead of buffering an unbounded number
/// of events; the task ends when the receiver is dropped.
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Shared state of the HTTP surface.
///
/// The app handle is shared with every handler; the token is `None` when the
/// operator did not configure one.
#[derive(Clone)]
pub struct ApiState {
    app: Arc<App>,
    token: Option<Arc<str>>,
}

impl ApiState {
    /// Builds the state. An empty or whitespace-only token means "no auth".
    #[must_use]
    pub fn new(app: Arc<App>, token: Option<String>) -> Self {
        let token = token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(Arc::<str>::from);
        Self { app, token }
    }

    /// The shared application service.
    #[must_use]
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// The configured bearer token, if any.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

/// Builds the router. `/health` is public, `/v1/*` is behind the optional token.
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route(
            "/agents",
            get(routes::list_agents).post(routes::create_agent),
        )
        .route("/agents/{id}/sessions", post(routes::create_session))
        .route("/agents/{id}/memories", get(routes::search_memories))
        .route("/tasks", post(routes::create_task))
        .route("/tasks/{id}/run", post(routes::run_task))
        .route("/runs/{id}", get(routes::get_run))
        .route("/runs/{id}/resume", post(routes::resume_run))
        .route("/runs/{id}/events", get(routes::run_events))
        .route("/runs/{id}/cancel", post(routes::cancel_run))
        .route(
            "/evolution/candidates",
            get(evolution::list_candidates).post(evolution::submit_candidate),
        )
        .route("/evolution/candidates/{id}", get(evolution::show_candidate))
        .route(
            "/evolution/candidates/{id}/evaluate",
            post(evolution::evaluate_candidate),
        )
        .route(
            "/evolution/candidates/{id}/promote",
            post(evolution::promote_candidate),
        )
        .route(
            "/evolution/promotions/{id}/rollback",
            post(evolution::rollback_promotion),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        .with_state(state);

    Router::new()
        .route("/health", get(routes::health))
        .nest("/v1", protected)
}

/// Environment-backed server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    /// Address to bind.
    pub addr: SocketAddr,
    /// Optional bearer token for `/v1/*`.
    pub token: Option<String>,
    /// Whether a non-loopback bind is permitted.
    pub allow_non_loopback: bool,
}

impl ApiConfig {
    /// Reads [`ADDR_ENV`], [`TOKEN_ENV`] and [`ALLOW_NON_LOOPBACK_ENV`].
    ///
    /// A malformed address is a permanent error, so a typo cannot silently fall
    /// back to the default port. A non-UTF-8 token is an error too: treating it
    /// as "no token" would silently disable authentication.
    pub fn from_env() -> Result<Self, SilynError> {
        let raw = match env::var(ADDR_ENV) {
            Ok(value) => value.trim().to_owned(),
            Err(env::VarError::NotPresent) => DEFAULT_ADDR.to_owned(),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(SilynError::Permanent(format!(
                    "{ADDR_ENV} is not valid UTF-8"
                )));
            }
        };
        let raw = if raw.is_empty() {
            DEFAULT_ADDR.to_owned()
        } else {
            raw
        };
        let addr: SocketAddr = raw.parse().map_err(|_| {
            SilynError::Permanent(format!(
                "{ADDR_ENV} must be an <ip>:<port> socket address, got `{raw}`"
            ))
        })?;
        let token = match env::var(TOKEN_ENV) {
            Ok(value) => {
                let value = value.trim().to_owned();
                if value.is_empty() { None } else { Some(value) }
            }
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(SilynError::Permanent(format!(
                    "{TOKEN_ENV} is not valid UTF-8"
                )));
            }
        };

        Ok(Self {
            addr,
            token,
            allow_non_loopback: env_flag(ALLOW_NON_LOOPBACK_ENV),
        })
    }

    /// Rejects a non-loopback bind unless it was explicitly allowed.
    pub fn validate(&self) -> Result<(), SilynError> {
        ensure_local_bind(self.addr, self.allow_non_loopback)
    }
}

/// Rejects a non-loopback address unless `allow_non_loopback` is set.
///
/// This runs before any socket is opened, so a misconfigured server fails
/// without ever listening on a routable interface.
pub fn ensure_local_bind(addr: SocketAddr, allow_non_loopback: bool) -> Result<(), SilynError> {
    if addr.ip().is_loopback() || allow_non_loopback {
        return Ok(());
    }
    Err(SilynError::Permanent(format!(
        "refusing to bind {addr}: the Silyn API is local-only; set {ALLOW_NON_LOOPBACK_ENV}=1 to override"
    )))
}

/// Serves the API until Ctrl-C, reading the bind flag from the environment.
///
/// The caller owns the [`App`] handle; this function only listens and routes.
pub async fn serve(
    app: Arc<App>,
    addr: SocketAddr,
    token: Option<String>,
) -> Result<(), SilynError> {
    serve_with(app, addr, token, env_flag(ALLOW_NON_LOOPBACK_ENV)).await
}

/// Serves the API until Ctrl-C with an explicit loopback override.
pub async fn serve_with(
    app: Arc<App>,
    addr: SocketAddr,
    token: Option<String>,
    allow_non_loopback: bool,
) -> Result<(), SilynError> {
    ensure_local_bind(addr, allow_non_loopback)?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| SilynError::Transient(format!("cannot bind {addr}: {error}")))?;
    axum::serve(listener, router(ApiState::new(app, token)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| SilynError::Transient(format!("http server stopped: {error}")))
}

/// Resolves on Ctrl-C so in-flight requests can finish and the port is freed.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// `1`, `true` and `yes` (case-insensitive) are truthy; anything else is not.
fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}
