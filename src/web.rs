//! The HTTP surface: a status page, two JSON endpoints, a lookup and a stream.
//!
//! There is no template engine. `/health` and `/lobbies` are serde_json, and the status
//! page is one string literal — a proc-macro template engine and a `templates/`
//! directory for a single page is not a trade this server needs to make.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::state::{AppState, BrowserEvent, PublicLobby};

/// How often the stream sends a comment when nothing is happening. Reverse proxies cut
/// an idle upstream — nginx's `proxy_read_timeout` defaults to 60 seconds — and a
/// browser then shows a lobby list that is silently frozen rather than empty.
const HEARTBEAT: Duration = Duration::from_secs(20);

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/lobbies", get(lobbies))
        .route("/lobbies/{id}/code", get(lobby_code))
        .route("/lobbies/stream", get(lobby_stream))
        .with_state(state)
}

async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let connections = state.connections.load(std::sync::atomic::Ordering::Relaxed);
    let lobbies = state.lobby_count();
    let name = state.name.as_deref().unwrap_or("AnotherCrewLink");
    Html(format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head><meta charset=\"utf-8\"><title>{name}</title>\n\
         <style>body{{font-family:system-ui,sans-serif;margin:3rem auto;max-width:32rem;\
         line-height:1.6}}dt{{font-weight:600}}dd{{margin:0 0 .75rem}}</style></head>\n\
         <body>\n\
         <h1>{name}</h1>\n\
         <p>A voice relay server for <a href=\"https://github.com/greluc/AnotherCrewLink\">\
         AnotherCrewLink</a>. Point the client at <code>{address}</code> under \
         Settings &rarr; Server.</p>\n\
         <dl>\n\
         <dt>Connected</dt><dd>{connections}</dd>\n\
         <dt>Lobbies</dt><dd>{lobbies}</dd>\n\
         </dl>\n\
         </body></html>\n",
        name = html_escape(name),
        address = html_escape(&state.public_address),
    ))
}

/// The status page interpolates two operator-supplied strings. Neither is attacker
/// controlled, but escaping them is cheaper than establishing that every time someone
/// reads this file.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Serialize)]
struct Health {
    uptime: f64,
    #[serde(rename = "connectionCount")]
    connection_count: i64,
    #[serde(rename = "lobbiesCount")]
    lobbies_count: usize,
    address: String,
    name: Option<String>,
    /// What this server refused or dropped since it started. The Node version had no
    /// equivalent, and a bounded buffer that drops silently is indistinguishable from a
    /// peer that never connects.
    counters: serde_json::Value,
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(Health {
        uptime: state.started.elapsed().as_secs_f64(),
        connection_count: state.connections.load(std::sync::atomic::Ordering::Relaxed),
        lobbies_count: state.lobby_count(),
        address: state.public_address.clone(),
        name: state.name.clone(),
        counters: state.counters.snapshot(),
    })
}

async fn lobbies(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(snapshot(&state))
}

fn snapshot(state: &AppState) -> Vec<PublicLobby> {
    state
        .public_lobbies
        .iter()
        .map(|entry| entry.value().clone())
        .collect()
}

/// A lobby code is the credential that gates entry to a game, so this must never be
/// cached — not by a browser, and not by the reverse proxy in front of it.
async fn lobby_code(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> impl IntoResponse {
    let headers = [(header::CACHE_CONTROL, "no-store")];
    match state.lobby_codes.get(&id) {
        Some(code) => {
            let code = code.clone();
            let joinable = state
                .public_lobbies
                .get(&code)
                .is_some_and(|lobby| lobby.is_public);
            if joinable {
                (
                    StatusCode::OK,
                    headers,
                    Json(serde_json::json!({ "code": code })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::GONE,
                    headers,
                    Json(serde_json::json!({ "error": "Lobby is not public anymore" })),
                )
                    .into_response()
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            headers,
            Json(serde_json::json!({ "error": "Lobby not found" })),
        )
            .into_response(),
    }
}

/// The lobby list as a stream.
///
/// A subscriber that arrives with no `Last-Event-ID`, or with one this server no longer
/// holds, is sent the whole list once and then follows along. That is the honest
/// behaviour for a feed of current state rather than a log: replaying an arbitrary
/// distance back would mean keeping every event forever.
async fn lobby_stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let last_seen = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let mut receiver = state.browser.subscribe();
    let replay = state.replay_since(last_seen);
    let initial = match replay {
        Some(events) => events,
        None => vec![(
            state.browser_position(),
            BrowserEvent::Snapshot(snapshot(&state)),
        )],
    };

    let stream = async_stream::stream! {
        for (id, event) in initial {
            yield Ok(sse_event(id, &event));
        }
        loop {
            match receiver.recv().await {
                Ok((id, event)) => yield Ok(sse_event(id, &event)),
                // A subscriber that cannot keep up is told where it is rather than
                // being handed a gap it cannot see.
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "a lobby stream subscriber fell behind");
                    yield Ok(Event::default().event("lagged").data(missed.to_string()));
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(HEARTBEAT).text("keep-alive"))
}

fn sse_event(id: u64, event: &BrowserEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned());
    Event::default().id(id.to_string()).data(data)
}
