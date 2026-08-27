//! The eleven socket events, ported from `src/index.ts`.
//!
//! Two behaviours from the Node server are carried over deliberately, because they were
//! bug fixes and a faithful-looking port would undo them:
//!
//! * `leave_room` announces `left`, so a peer can tell a departure from a connection
//!   that broke while both players were still in the lobby.
//! * `leave` clears the socket's lobby, so the disconnect that follows does not run the
//!   cleanup a second time and announce the same departure twice.
//!
//! One behaviour is new and is enforced from the first commit rather than arriving later
//! as hardening: the signal envelope. A `signal` is relayed only to a socket in the
//! sender's own lobby, never back to the sender, and never above a size cap. The Node
//! server relayed to whatever target a sender named, which let anyone who knew a
//! six-character lobby code address any socket on the server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde::Deserialize;
use serde_json::value::RawValue;
use socketioxide::SocketIo;
use socketioxide::extract::{AckSender, Data, SocketRef, State, TryData};
// For `.with(..)` on the connect handler: the middleware half of the split in
// `register` below, and the whole reason a first event cannot be dropped.
use socketioxide::handler::ConnectHandler;
use socketioxide::socket::Sid;

use crate::state::{
    AppState, BrowserEvent, Bucket, Client, GAME_STATE_LOBBY, JOIN_RATE, LOBBY_RATE, Limits,
    Membership, PublicLobbyInput, RADIO_RATE, SIGNAL_RATE, VAD_RATE,
};

/// The room used for the lobby browser. This one is a socketioxide room on purpose: it
/// is a broadcast group with no membership predicate attached to it, which is exactly
/// what rooms are good at, and broadcasting through one serialises the payload once.
const BROWSER_ROOM: &str = "lobbybrowser";

/// A signal payload above this is refused. The largest legitimate one is an SDP offer,
/// which is a few kilobytes.
const MAX_SIGNAL_BYTES: usize = 64 * 1024;

/// Renders a payload once so that a fan-out hands every recipient the same bytes.
///
/// A `RawValue` is not tuple-like, so the encoder wraps it as a single argument and
/// emits it verbatim. Events that carry two arguments pass a tuple whose second element
/// is one of these.
fn render<T: serde::Serialize>(value: &T) -> Option<Box<RawValue>> {
    match serde_json::to_string(value) {
        Ok(text) => RawValue::from_string(text).ok(),
        Err(err) => {
            tracing::error!(%err, "could not render an outbound payload");
            None
        }
    }
}

/// Emits to one socket and accounts for the two ways it can fail. A full buffer is the
/// one that matters: it means the recipient has stopped reading, and counting it is what
/// keeps that visible instead of presenting as a peer that never connects.
fn deliver<T: serde::Serialize + ?Sized>(
    io: &SocketIo,
    state: &AppState,
    sid: Sid,
    event: &str,
    payload: &T,
) {
    let Some(socket) = io.get_socket(sid) else {
        return;
    };
    if let Err(err) = socket.emit(event, payload) {
        state
            .counters
            .dropped_full_buffer
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(%sid, event, %err, "dropped an event for one socket");
    }
}

/// Removes a socket from its lobby and tells the others.
///
/// Returns the code it left, so the caller can decide what else to clean up.
async fn leave_room(io: &SocketIo, state: &Arc<AppState>, sid: Sid, code: &str) {
    let mut peers = Vec::new();
    let mut lobby_now_empty = false;
    let mut host_released = false;

    if let Some(mut lobby) = state.lobbies.get_mut(code) {
        lobby.members.remove(&sid);
        if lobby.host_sid == Some(sid) {
            // First claimer wins, and holds it until they leave. Releasing it here is
            // what lets the next claim succeed.
            lobby.host_sid = None;
            lobby.host_id = None;
            host_released = true;
        }
        peers = lobby.members.keys().copied().collect();
        lobby_now_empty = lobby.members.is_empty();
    }

    // The guard is gone before anything is sent.
    let sid_text = sid.to_string();
    for peer in &peers {
        deliver(io, state, *peer, "left", &sid_text);
    }
    if host_released {
        for peer in &peers {
            deliver(io, state, *peer, "setHost", &-1i64);
        }
    }

    if lobby_now_empty {
        state.lobbies.remove(code);
        remove_public_lobby(io, state, code).await;
    }
}

async fn remove_public_lobby(io: &SocketIo, state: &Arc<AppState>, code: &str) {
    if let Some((_, lobby)) = state.public_lobbies.remove(code) {
        state.lobby_codes.remove(&lobby.id);
        let _ = io.to(BROWSER_ROOM).emit("remove_lobby", &lobby.id).await;
        state.publish(BrowserEvent::RemoveLobby(lobby.id));
    }
}

#[derive(Debug, Deserialize)]
pub struct SignalIn {
    /// The raw bytes, deliberately not a `serde_json::Value`.
    ///
    /// This is the one field on this server that carries an arbitrary attacker-chosen
    /// document, and the WebSocket transport does not bound it -- `max_payload` in
    /// `main.rs` is applied by engineioxide to the polling transport only, and the
    /// WebSocket side leaves tungstenite's 64 MiB default in place. There is no knob for
    /// it in socketioxide 0.18.6; `transport/ws.rs` builds a `WebSocketConfig` and sets
    /// only `read_buffer_size`.
    ///
    /// Parsing that into a `Value` built a node tree several times the size of the bytes
    /// that arrived, before anything had looked at how big it was. A `RawValue` keeps the
    /// original slice, so the size check below happens on what was actually received and
    /// nothing is walked, allocated per node, or re-serialised on the way out.
    pub data: Box<RawValue>,
    pub to: String,
}

/// Wires the namespace up in two stages, and the split is load-bearing.
///
/// socketioxide sends the CONNECT packet **before** it calls the connect handler, and an
/// async connect handler is `tokio::spawn`ed rather than awaited — `Namespace::connect` in
/// socketioxide 0.18.6 does `socket.send(Packet::connect(..))`, then `set_connected(true)`,
/// then `self.handler.call(..)`, and `call` for an async closure ends in `tokio::spawn`.
/// So between a client being told it is connected and this server registering that
/// socket's event handlers there is a gap of whatever length the scheduler decides.
///
/// An event that lands in that gap is **dropped in silence**. There is no handler for it,
/// so nothing acks it, nothing errors, nothing disconnects — the client waits for an
/// answer that was never going to come. The client conformance test in the desktop
/// repository hit exactly this: a `join` emitted the instant CONNECT arrived, then thirty
/// seconds of heartbeats and no `setClients`. It reproduces every time if you sleep at the
/// top of the connect handler.
///
/// Middleware runs on the other side of that line. `Namespace::connect` awaits
/// `call_middleware` **before** it sends the CONNECT packet, so anything registered there
/// is in place before the client can physically emit anything. That is where the handlers
/// belong, and the admission bookkeeping with them: the join handler updates
/// `state.members` through `get_mut`, which silently does nothing when the row is absent.
///
/// What is left for the connect handler is the one thing that cannot happen earlier —
/// middleware may not send to a socket that is not connected yet — which is the peer
/// config.
pub fn register(io: &SocketIo, state: Arc<AppState>) {
    let io_for_handlers = io.clone();
    io.ns(
        "/",
        (async move |socket: SocketRef, State(state): State<Arc<AppState>>| {
            on_connect(&socket, &state);
        })
        .with(
            async move |socket: SocketRef, State(state): State<Arc<AppState>>| {
                let io = io_for_handlers.clone();
                admit(&socket, &io, &state);
                Ok::<(), std::convert::Infallible>(())
            },
        ),
    );
    // The state extractor above resolves through socketioxide's own registry; this keeps
    // the handle for the paths that need it outside a handler.
    let _ = state;
}

/// Everything a client's first event depends on, done before the client is told it may
/// send one.
///
/// The counter is incremented here rather than in the connect handler because
/// `on_disconnect` — registered below — decrements it, and a socket that disconnects
/// immediately would otherwise subtract a count that had not been added yet.
///
/// One case is not balanced, and it is worth naming rather than leaving to be discovered:
/// if socketioxide then fails to send the CONNECT packet it calls `remove_socket` and
/// closes the transport without running the disconnect handler, so this row and this count
/// leak. That happens only when the client has already gone between the handshake and the
/// connect packet, and one stale row on a dead connection is a smaller thing than an event
/// dropped on a live one.
fn admit(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    state.connections.fetch_add(1, Ordering::Relaxed);
    state.members.insert(socket.id, Membership::default());

    on_join(socket, io, state);
    on_set_host(socket, io, state);
    on_id(socket, io, state);
    on_leave(socket, io, state);
    on_vad(socket, io, state);
    on_impostor_radio(socket, io, state);
    on_join_lobby(socket, state);
    on_lobby(socket, io, state);
    on_remove_lobby(socket, io, state);
    on_signal(socket, io, state);
    on_lobby_browser(socket, state);
    on_disconnect(socket, io, state);
}

/// The only work that needs a socket the client already knows about.
fn on_connect(socket: &SocketRef, state: &Arc<AppState>) {
    tracing::info!(
        connections = state.connections.load(Ordering::Relaxed),
        lobbies = state.lobby_count(),
        "socket connected"
    );

    // Minted for this client at this moment: the credential carries an expiry, and the
    // socket id goes into the username so no two clients hold the same one. It used to be
    // the expiry alone, which everybody connecting in the same second shared -- a probe
    // against the live server found exactly that. One HMAC over a couple of dozen bytes.
    let sid = socket.id.to_string();
    if socket
        .emit("clientPeerConfig", &state.peer_config.issue(&sid))
        .is_err()
    {
        tracing::warn!(sid = %socket.id, "could not send the peer config");
    }
}

/// Spends a token from one of the sender's buckets.
///
/// Over the limit the caller returns without doing the work, and the drop is counted on
/// `/health` as `refusedRateLimited`. Deliberately not a disconnect: these limits sit far
/// above what the shipped client does, but a client that stutters past a burst -- a laggy
/// machine, a resumed laptop, a garbage-collection pause -- must not lose its call over
/// it. What has to be stopped is a sender that never stops, and dropping does that.
///
/// A socket with no membership row is let through: it has not been admitted yet, and
/// every handler checks that for itself and refuses more precisely than this could.
fn within_limit(
    state: &Arc<AppState>,
    sid: Sid,
    pick: impl FnOnce(&mut Limits) -> &mut Bucket,
    rate: (f64, f64),
) -> bool {
    let now = Instant::now();
    // The guard is released by the end of this statement, before any caller goes on to
    // touch `lobbies`. Holding both maps at once, in either order, is how this would
    // deadlock.
    let allowed = match state.members.get_mut(&sid) {
        Some(mut member) => pick(&mut member.limits).allow(rate.0, rate.1, now),
        None => true,
    };
    if !allowed {
        state
            .counters
            .refused_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }
    allowed
}

fn on_join(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "join",
        async move |socket: SocketRef,
                    TryData(payload): TryData<(String, i64, i64, Option<bool>)>| {
            let Ok((code, player_id, client_id, is_host)) = payload else {
                state
                    .counters
                    .refused_malformed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(sid = %socket.id, "invalid join command");
                let _ = socket.disconnect();
                return;
            };

            let sid = socket.id;
            if !within_limit(&state, sid, |l| &mut l.join, JOIN_RATE) {
                return;
            }
            let client = Client {
                player_id,
                client_id,
            };

            // Whatever this socket was in before, it is not in it any more.
            let previous = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone());
            if let Some(previous) = previous
                && *previous != *code
            {
                leave_room(&io, &state, sid, &previous).await;
            }

            // Everyone already here, and whether the host is claimed.
            let (others, host_id) = {
                let mut lobby = state.lobbies.entry(code.clone()).or_default();

                // First claimer wins. A later socket claiming to be host is ignored
                // until the current holder leaves, which is what stops a lobby member
                // from taking the host's authority over lobby settings.
                if is_host == Some(true) && lobby.host_id.is_none() {
                    lobby.host_id = Some(client_id);
                    lobby.host_sid = Some(sid);
                }

                let others: HashMap<String, Client> = lobby
                    .members
                    .iter()
                    .filter(|(other, _)| **other != sid)
                    .filter_map(|(other, member)| {
                        member.client.map(|client| (other.to_string(), client))
                    })
                    .collect();

                lobby.members.entry(sid).or_default().client = Some(client);
                (others, lobby.host_id_or_unset())
            };

            if let Some(mut membership) = state.members.get_mut(&sid) {
                membership.code = Some(Arc::from(code.as_str()));
                membership.client = Some(client);
            }

            let peers = state.peers_in(&code, sid);
            let rendered = render(&client);
            let sid_text = sid.to_string();

            for peer in &peers {
                if let Some(rendered) = rendered.as_deref() {
                    deliver(&io, &state, *peer, "join", &(&sid_text, rendered));
                }
                deliver(&io, &state, *peer, "setHost", &host_id);
            }

            let _ = socket.emit("setHost", &host_id);
            let _ = socket.emit("setClients", &others);
        },
    );
}

fn on_set_host(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "setHost",
        async move |socket: SocketRef, TryData(payload): TryData<(String, i64)>| {
            let Ok((code, client_id)) = payload else {
                tracing::error!(sid = %socket.id, "invalid setHost command");
                return;
            };
            let sid = socket.id;
            let in_this_lobby = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone())
                .is_some_and(|current| *current == *code);
            if !in_this_lobby {
                tracing::warn!(sid = %sid, code, "setHost for a lobby this socket is not in");
                return;
            }

            let accepted = {
                match state.lobbies.get_mut(&code) {
                    None => false,
                    Some(mut lobby) => match lobby.host_sid {
                        // Unclaimed, or already ours: accept.
                        None => {
                            lobby.host_id = Some(client_id);
                            lobby.host_sid = Some(sid);
                            true
                        }
                        Some(holder) if holder == sid => {
                            lobby.host_id = Some(client_id);
                            true
                        }
                        Some(_) => false,
                    },
                }
            };

            if !accepted {
                tracing::warn!(sid = %sid, code, "refused a host claim; the lobby already has one");
                return;
            }

            for peer in state.peers_in(&code, sid) {
                deliver(&io, &state, peer, "setHost", &client_id);
            }
        },
    );
}

fn on_id(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "id",
        async move |socket: SocketRef, TryData(payload): TryData<(i64, i64)>| {
            let Ok((player_id, client_id)) = payload else {
                state
                    .counters
                    .refused_malformed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(sid = %socket.id, "invalid id command");
                let _ = socket.disconnect();
                return;
            };
            let sid = socket.id;
            let client = Client {
                player_id,
                client_id,
            };

            let code = {
                let Some(mut membership) = state.members.get_mut(&sid) else {
                    return;
                };
                if let Some(previous) = membership.client
                    && previous.client_id != client_id
                {
                    // Recorded, not acted on: the server cannot tell a spoof from a
                    // client that legitimately changed identity between games.
                    tracing::warn!(
                        sid = %sid,
                        from = previous.client_id,
                        to = client_id,
                        "socket changed the client id it claims"
                    );
                }
                membership.client = Some(client);
                membership.code.clone()
            };

            let Some(code) = code else { return };

            if let Some(mut lobby) = state.lobbies.get_mut(code.as_ref()) {
                lobby.members.entry(sid).or_default().client = Some(client);
            }

            let rendered = render(&client);
            let sid_text = sid.to_string();
            for peer in state.peers_in(&code, sid) {
                if let Some(rendered) = rendered.as_deref() {
                    deliver(&io, &state, peer, "setClient", &(&sid_text, rendered));
                }
            }
        },
    );
}

fn on_leave(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on("leave", async move |socket: SocketRef| {
        let sid = socket.id;
        let code = state
            .members
            .get_mut(&sid)
            .and_then(|mut member| member.code.take());
        if let Some(code) = code {
            leave_room(&io, &state, sid, code.as_ref()).await;
        }
    });
}

/// The impostor radio, relayed to the rest of the lobby.
///
/// **A 2.x event.** 1.x carries this over the WebRTC data channel and never sends or
/// receives it here, so adding it takes nothing away from anybody: a mixed lobby degrades
/// exactly as far as it did before, and a lobby of 2.x clients gets a radio that works.
///
/// Deliberately the same shape as `VAD` -- one peer, one boolean, relayed to the lobby --
/// because it is the same kind of message, and two shapes would be two parsers to keep in
/// step for no reason.
///
/// **This server does not decide who may claim it.** Being an impostor is a fact about the
/// game, which this server has never read and must not start reading: it would have to be
/// told, and a client that can tell it is a client that can lie to it. Both ends check
/// instead -- the sender before claiming, the receiver before lifting any distance rule --
/// so a lie is believed by nobody. Relaying it is all that is safe to do here, and all that
/// is needed.
fn on_impostor_radio(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "impostorRadio",
        async move |socket: SocketRef, TryData(on_radio): TryData<bool>| {
            let Ok(on_radio) = on_radio else { return };
            let sid = socket.id;
            if !within_limit(&state, sid, |l| &mut l.radio, RADIO_RATE) {
                return;
            }
            let Some((code, client)) = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone().zip(member.client))
            else {
                return;
            };

            let payload = serde_json::json!({
                "onRadio": on_radio,
                "client": client,
                "socketId": sid.to_string(),
            });
            let Some(rendered) = render(&payload) else {
                return;
            };
            for peer in state.peers_in(&code, sid) {
                deliver(&io, &state, peer, "impostorRadio", &*rendered);
            }
        },
    );
}

fn on_vad(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "VAD",
        async move |socket: SocketRef, TryData(activity): TryData<bool>| {
            let Ok(activity) = activity else { return };
            let sid = socket.id;
            if !within_limit(&state, sid, |l| &mut l.vad, VAD_RATE) {
                return;
            }
            let Some((code, client)) = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone().zip(member.client))
            else {
                return;
            };

            let payload = serde_json::json!({
                "activity": activity,
                "client": client,
                "socketId": sid.to_string(),
            });
            let Some(rendered) = render(&payload) else {
                return;
            };
            for peer in state.peers_in(&code, sid) {
                deliver(&io, &state, peer, "VAD", &*rendered);
            }
        },
    );
}

fn on_join_lobby(socket: &SocketRef, state: &Arc<AppState>) {
    let state = state.clone();
    socket.on(
        "join_lobby",
        async move |TryData(id): TryData<u64>, ack: AckSender| {
            let Ok(id) = id else {
                let _ = ack.send(&(1, "Lobby not found :C"));
                return;
            };
            let code = state.lobby_codes.get(&id).map(|entry| entry.clone());
            let lobby = code
                .as_ref()
                .and_then(|code| state.public_lobbies.get(code).map(|l| l.clone()));

            match (code, lobby) {
                (Some(code), Some(lobby))
                    if lobby.is_public && lobby.game_state == GAME_STATE_LOBBY =>
                {
                    let _ = ack.send(&(0, code, lobby.server.clone(), lobby));
                }
                // Known but not joinable. The Node version fell through from here into
                // the not-found reply as well, invoking the acknowledgement twice.
                (Some(_), Some(_)) => {
                    let _ = ack.send(&(1, "Lobby is not public anymore"));
                }
                _ => {
                    let _ = ack.send(&(1, "Lobby not found :C"));
                }
            }
        },
    );
}

fn on_lobby(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "lobby",
        async move |socket: SocketRef, TryData(payload): TryData<(String, PublicLobbyInput)>| {
            let Ok((code, input)) = payload else {
                tracing::error!(sid = %socket.id, "invalid lobby command");
                return;
            };
            let sid = socket.id;
            if !within_limit(&state, sid, |l| &mut l.lobby, LOBBY_RATE) {
                return;
            }
            let in_this_lobby = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone())
                .is_some_and(|current| *current == *code);
            if !in_this_lobby {
                tracing::warn!(sid = %sid, code, "lobby command for a lobby this socket is not in");
                return;
            }

            if !input.is_public() {
                remove_public_lobby(&io, &state, &code).await;
                return;
            }

            let previous = state.public_lobbies.get(&code).map(|entry| entry.clone());
            let id = previous
                .as_ref()
                .map_or_else(|| state.next_lobby_id(), |p| p.id);
            let now = now_millis();
            let incoming_state = input
                .game_state
                .as_ref()
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            // The timestamp only moves when the lobby crosses into or out of the
            // waiting state, so a browser can show how long it has been there.
            let state_time = match &previous {
                Some(previous)
                    if (previous.game_state == GAME_STATE_LOBBY)
                        == (incoming_state == GAME_STATE_LOBBY) =>
                {
                    previous.state_time
                }
                _ => now,
            };

            let lobby = input.sanitise(id, state_time);
            state.lobby_codes.insert(id, code.clone());
            state.public_lobbies.insert(code, lobby.clone());
            let _ = io.to(BROWSER_ROOM).emit("update_lobby", &lobby).await;
            state.publish(BrowserEvent::UpdateLobby(lobby));
        },
    );
}

fn on_remove_lobby(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "remove_lobby",
        async move |socket: SocketRef, TryData(code): TryData<String>| {
            let Ok(code) = code else { return };
            let sid = socket.id;
            let in_this_lobby = state
                .members
                .get(&sid)
                .and_then(|member| member.code.clone())
                .is_some_and(|current| *current == *code);
            if !in_this_lobby {
                tracing::warn!(sid = %sid, code, "remove_lobby for a lobby this socket is not in");
                return;
            }
            remove_public_lobby(&io, &state, &code).await;
        },
    );
}

fn on_signal(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on(
        "signal",
        async move |socket: SocketRef, TryData(signal): TryData<SignalIn>| {
            let sid = socket.id;
            if !within_limit(&state, sid, |l| &mut l.signal, SIGNAL_RATE) {
                return;
            }
            let Ok(signal) = signal else {
                state
                    .counters
                    .refused_malformed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(sid = %sid, "invalid signal command");
                let _ = socket.disconnect();
                return;
            };

            // The envelope. Three rules, all of them cheap, and together they are what
            // stops a socket from addressing anything it likes on this server.
            let Ok(target) = signal.to.parse::<Sid>() else {
                state.counters.refused_signals.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(sid = %sid, to = signal.to, "refused a signal: the target is not a socket id");
                return;
            };
            if target == sid {
                state.counters.refused_signals.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(sid = %sid, "refused a signal addressed to its own sender");
                return;
            }
            if !state.are_co_members(sid, target) {
                state.counters.refused_signals.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(sid = %sid, %target, "refused a signal: the target is not in the sender's lobby");
                return;
            }

            // Checked on the bytes that arrived, before anything is built from them. The
            // envelope above is cheap and runs first; this is the second thing that can
            // refuse, and it refuses without ever having allocated a copy.
            if signal.data.get().len() > MAX_SIGNAL_BYTES {
                state
                    .counters
                    .refused_oversize
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(sid = %sid, bytes = signal.data.get().len(), "refused an oversized signal");
                return;
            }

            let payload = serde_json::json!({
                "data": signal.data,
                "from": sid.to_string(),
            });
            let Some(rendered) = render(&payload) else {
                return;
            };

            deliver(&io, &state, target, "signal", &*rendered);
        },
    );
}

fn on_lobby_browser(socket: &SocketRef, state: &Arc<AppState>) {
    let state = state.clone();
    socket.on(
        "lobbybrowser",
        async move |socket: SocketRef, TryData(open): TryData<bool>| {
            let open = open.unwrap_or(false);
            if let Some(mut membership) = state.members.get_mut(&socket.id) {
                membership.watching_lobbies = open;
            }
            if open {
                socket.join(BROWSER_ROOM);
                let lobbies: Vec<_> = state
                    .public_lobbies
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect();
                // A Vec is always one argument, which is the shape the client reads.
                let _ = socket.emit("new_lobbies", &lobbies);
            } else {
                socket.leave(BROWSER_ROOM);
            }
        },
    );
}

fn on_disconnect(socket: &SocketRef, io: &SocketIo, state: &Arc<AppState>) {
    let io = io.clone();
    let state = state.clone();
    socket.on_disconnect(async move |socket: SocketRef| {
        let sid = socket.id;
        if let Some((_, membership)) = state.members.remove(&sid)
            && let Some(code) = membership.code
        {
            leave_room(&io, &state, sid, code.as_ref()).await;
        }
        state.connections.fetch_sub(1, Ordering::Relaxed);
        tracing::info!(
            connections = state.connections.load(Ordering::Relaxed),
            lobbies = state.lobby_count(),
            "socket disconnected"
        );
    });
}

pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Kept out of the handler signatures above so the extractor list stays readable.
#[allow(dead_code)]
type Ignored = Data<serde_json::Value>;
