//! AnotherCrewLink signalling server.
//!
//! TLS terminates at a reverse proxy and this binds to the loopback interface by
//! default, which is what keeps a crypto stack out of this binary entirely.

mod config;
mod socket;
mod state;
mod web;

use std::sync::Arc;
use std::time::Duration;

use socketioxide::{SocketIo, TransportType};
use tower_http::LatencyUnit;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};

use crate::config::{PeerConfigFile, Settings};
use crate::state::AppState;

/// A socket that stops reading fills this and its emits then fail, which is counted on
/// `/health`. Unbounded would be a denial of service; this is roughly a second of the
/// busiest event stream a lobby produces.
const SOCKET_BUFFER: usize = 128;

/// The handshake advertises this, and socketioxide enforces it on the polling transport.
/// It does **not** enforce it on the WebSocket transport, which is the accepted risk
/// recorded in `docs/rust-port/04-implementation-plan.md`; the per-event size check in
/// `socket.rs` is what actually bounds a relayed payload.
const MAX_PAYLOAD: u64 = 64 * 1024;

/// A socket that connects and never joins a namespace holds a slot until this expires.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aucl_server=info".into()),
        )
        .init();

    let settings = Settings::from_env();
    let peer_config =
        PeerConfigFile::load(&settings.peer_config_path).resolve(settings.hostname.as_deref());

    tracing::info!(
        ice_servers = peer_config.ice_servers.len(),
        force_relay_only = peer_config.force_relay_only,
        "peer configuration loaded"
    );

    let state = Arc::new(AppState::new(
        peer_config,
        settings.name.clone(),
        settings.public_address.clone(),
    ));

    let (layer, io) = SocketIo::builder()
        // Both shipping clients connect with `transports: ['websocket']`. Refusing
        // polling removes a transport nobody legitimate uses, and with it the advisory
        // history that transport carries.
        .transports([TransportType::Websocket])
        .connect_timeout(CONNECT_TIMEOUT)
        .max_buffer_size(SOCKET_BUFFER)
        .max_payload(MAX_PAYLOAD)
        .with_state(state.clone())
        .build_layer();

    socket::register(&io, state.clone());

    // CORS sits on the HTTP routes a browser fetches with XHR, and nowhere else. The
    // socket.io layer is the outermost wrapper below, so it answers its own path before
    // any of this is reached: a WebSocket upgrade is not a CORS request and needs no
    // server-side permission, and an Origin allow-list there would restrict only
    // browsers while being the one thing that can take the overlay off the air.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([axum::http::Method::GET]);

    let app = web::router(state.clone())
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(16 * 1024))
        // Slow-body, which hyper's header timeout does not cover.
        .layer(RequestBodyTimeoutLayer::new(Duration::from_secs(15)))
        .layer(CatchPanicLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .on_response(DefaultOnResponse::new().latency_unit(LatencyUnit::Millis)),
        )
        .layer(layer);

    let listener = tokio::net::TcpListener::bind(settings.bind).await?;
    tracing::info!(address = %settings.bind, "AnotherCrewLink server started");

    let shutdown_io = io.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            tracing::info!("shutting down");
            // Closing the namespaces first is not optional: axum's graceful shutdown
            // waits for in-flight connections, and a WebSocket never completes on its
            // own, so every deployment would need a kill signal instead.
            shutdown_io.close().await;
        })
        .await?;

    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(err) => tracing::error!(%err, "could not listen for SIGTERM"),
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}
