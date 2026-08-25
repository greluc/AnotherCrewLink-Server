//! The lobby registry, owned rather than borrowed from socketioxide's rooms.
//!
//! Rooms hold socketioxide sockets and nothing else, so they cannot answer "is socket X
//! in the lobby socket Y is in". That predicate is what the signal envelope rules are
//! built on, which is why the registry lives here.
//!
//! Three rules govern this module, and they are the reason it exists:
//!
//! 1. **Delivery is bounded.** Every socket has a fixed-size outbound buffer, configured
//!    in `main`. A socket that stops reading fills its buffer and its emits then fail
//!    with `InternalChannelFull`, which is counted and logged rather than discarded.
//!    Unbounded buffering would be a denial of service the Node server does not have;
//!    bounded and silent would surface as peers that never connect.
//! 2. **Payloads are serialised once per event, not once per recipient.** Fan-out builds
//!    one `RawValue` and hands the same rendered bytes to every socket.
//! 3. **No lock is held across an await.** Every map guard is dropped before anything is
//!    sent. The helpers here collect their targets first and return them.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use socketioxide::socket::Sid;
use tokio::sync::broadcast;

use crate::config::PeerConfigProvider;

/// The identity a client claims for itself. The server does not verify it — that is
/// inherited from CrewLink's design and is out of scope here — but it records it, and
/// logs when a socket contradicts its earlier claim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Client {
    #[serde(rename = "playerId")]
    pub player_id: i64,
    #[serde(rename = "clientId")]
    pub client_id: i64,
}

/// A lobby as the public browser sees it. The field names are the wire's: the shipped
/// clients read `current_players` and `isPublic` from the same object, and this mixture
/// of conventions is theirs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicLobby {
    pub id: u64,
    pub title: String,
    pub host: String,
    pub current_players: i64,
    pub max_players: i64,
    pub language: String,
    pub mods: String,
    #[serde(rename = "isPublic")]
    pub is_public: bool,
    pub server: String,
    #[serde(rename = "gameState")]
    pub game_state: i64,
    #[serde(rename = "stateTime")]
    pub state_time: i64,
}

/// What a client sends when it publishes or updates its lobby. Every field is optional
/// and every field is whatever the sender chose, so each one is coerced rather than
/// trusted. Serde stops here at the shape; `sanitise` handles the rest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PublicLobbyInput {
    pub title: Option<serde_json::Value>,
    pub host: Option<serde_json::Value>,
    pub current_players: Option<serde_json::Value>,
    pub max_players: Option<serde_json::Value>,
    pub language: Option<serde_json::Value>,
    pub mods: Option<serde_json::Value>,
    #[serde(rename = "isPublic")]
    pub is_public: Option<serde_json::Value>,
    #[serde(rename = "isPublic2")]
    pub is_public2: Option<serde_json::Value>,
    pub server: Option<serde_json::Value>,
    #[serde(rename = "gameState")]
    pub game_state: Option<serde_json::Value>,
}

/// The game state values the client reports. Only `Lobby` is load-bearing here: it is
/// the state in which a public lobby is joinable.
pub const GAME_STATE_LOBBY: i64 = 0;

/// Truncates to a character count, not a byte count. The Node version used
/// `String.prototype.substring`, which counts UTF-16 code units; counting characters is
/// the closest thing that cannot split a multi-byte sequence in half.
fn as_text(value: Option<&serde_json::Value>, max_len: usize) -> String {
    match value.and_then(|v| v.as_str()) {
        Some(text) => text.chars().take(max_len).collect(),
        None => String::new(),
    }
}

fn as_count(value: Option<&serde_json::Value>) -> i64 {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n.trunc().max(0.0) as i64)
        .unwrap_or(0)
}

fn as_flag(value: Option<&serde_json::Value>) -> bool {
    value.and_then(serde_json::Value::as_bool).unwrap_or(false)
}

impl PublicLobbyInput {
    pub fn is_public(&self) -> bool {
        as_flag(self.is_public.as_ref()) || as_flag(self.is_public2.as_ref())
    }

    pub fn sanitise(&self, id: u64, state_time: i64) -> PublicLobby {
        let title = as_text(self.title.as_ref(), 20);
        PublicLobby {
            id,
            title: if title.is_empty() {
                "ERROR".to_owned()
            } else {
                title
            },
            host: as_text(self.host.as_ref(), 10),
            current_players: as_count(self.current_players.as_ref()),
            max_players: as_count(self.max_players.as_ref()),
            language: as_text(self.language.as_ref(), 5),
            mods: as_text(self.mods.as_ref(), 20).to_uppercase(),
            is_public: self.is_public(),
            server: as_text(self.server.as_ref(), 100),
            game_state: as_count(self.game_state.as_ref()),
            state_time,
        }
    }
}

/// A member of one lobby.
#[derive(Debug, Clone, Default)]
pub struct Member {
    pub client: Option<Client>,
}

/// One lobby, and everything the envelope rules need to reason about it.
#[derive(Debug, Default)]
pub struct Lobby {
    pub members: HashMap<Sid, Member>,
    /// First claimer wins, and holds it until they leave. `None` means nobody has
    /// claimed it, which the client reads as -1.
    pub host_id: Option<i64>,
    pub host_sid: Option<Sid>,
}

impl Lobby {
    pub fn host_id_or_unset(&self) -> i64 {
        self.host_id.unwrap_or(-1)
    }
}

/// What a socket is currently part of.
#[derive(Debug, Clone, Default)]
pub struct Membership {
    pub code: Option<String>,
    pub client: Option<Client>,
    pub watching_lobbies: bool,
}

/// Counters surfaced on `/health`, so the things this server refuses or drops are
/// visible without attaching a debugger to it.
#[derive(Debug, Default)]
pub struct Counters {
    /// Emits that could not be queued because the recipient's buffer was full.
    pub dropped_full_buffer: AtomicU64,
    /// `signal` messages refused by the envelope rules.
    pub refused_signals: AtomicU64,
    /// Payloads refused for exceeding the size cap.
    pub refused_oversize: AtomicU64,
    /// Sockets disconnected for sending a malformed command.
    pub refused_malformed: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "droppedFullBuffer": self.dropped_full_buffer.load(Ordering::Relaxed),
            "refusedSignals": self.refused_signals.load(Ordering::Relaxed),
            "refusedOversize": self.refused_oversize.load(Ordering::Relaxed),
            "refusedMalformed": self.refused_malformed.load(Ordering::Relaxed),
        })
    }
}

/// What the lobby browser stream carries. The socket events of the same names carry the
/// same payloads, so a browser reading the stream and a client listening on the socket
/// see the same sequence.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", content = "lobby")]
pub enum BrowserEvent {
    /// The whole list, sent to a subscriber that cannot be resumed from where it left
    /// off. A stream of current state has to be able to start somewhere.
    Snapshot(Vec<PublicLobby>),
    UpdateLobby(PublicLobby),
    RemoveLobby(u64),
}

/// How much of the stream is kept for subscribers that reconnect. Twenty seconds of a
/// busy server, which is the window a dropped connection realistically returns inside.
const BROWSER_LOG_LEN: usize = 256;

pub struct AppState {
    pub started: Instant,
    pub name: Option<String>,
    pub public_address: String,
    pub peer_config: PeerConfigProvider,

    pub lobbies: DashMap<String, Lobby>,
    pub members: DashMap<Sid, Membership>,
    pub public_lobbies: DashMap<String, PublicLobby>,
    pub lobby_codes: DashMap<u64, String>,

    pub lobby_seq: AtomicU64,
    pub connections: AtomicI64,
    pub counters: Counters,

    /// Fan-out for `/lobbies/stream`. Bounded, like everything else that can be fed
    /// faster than it is read; a subscriber that falls behind is told it lagged rather
    /// than being allowed to hold the sender's memory.
    pub browser: broadcast::Sender<(u64, BrowserEvent)>,
    browser_seq: AtomicU64,
    /// The recent tail, so a subscriber returning with a `Last-Event-ID` resumes rather
    /// than restarting. Poisoning is handled rather than unwrapped: one panic while
    /// holding this lock must not take the lobby list off the air permanently.
    browser_log: Mutex<VecDeque<(u64, BrowserEvent)>>,
}

impl AppState {
    pub fn new(
        peer_config: PeerConfigProvider,
        name: Option<String>,
        public_address: String,
    ) -> Self {
        let (browser, _) = broadcast::channel(BROWSER_LOG_LEN);
        Self {
            started: Instant::now(),
            name,
            public_address,
            peer_config,
            lobbies: DashMap::new(),
            members: DashMap::new(),
            public_lobbies: DashMap::new(),
            lobby_codes: DashMap::new(),
            lobby_seq: AtomicU64::new(0),
            connections: AtomicI64::new(0),
            counters: Counters::default(),
            browser,
            browser_seq: AtomicU64::new(0),
            browser_log: Mutex::new(VecDeque::with_capacity(BROWSER_LOG_LEN)),
        }
    }

    fn browser_log(&self) -> std::sync::MutexGuard<'_, VecDeque<(u64, BrowserEvent)>> {
        self.browser_log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The id a subscriber starting now should report as its position.
    pub fn browser_position(&self) -> u64 {
        self.browser_seq.load(Ordering::Relaxed)
    }

    /// Everything after `last_seen`, or `None` when that point is no longer held and the
    /// subscriber has to be given a fresh snapshot instead.
    pub fn replay_since(&self, last_seen: Option<u64>) -> Option<Vec<(u64, BrowserEvent)>> {
        let last_seen = last_seen?;

        // `last_seen` came out of a `Last-Event-ID` request header, so it is whatever the
        // client typed rather than anything this server issued. Two ways it can be
        // outside the log, and both have to end in a snapshot:
        //
        // * **Behind it.** The ordinary case -- a subscriber away longer than the log is
        //   deep. This was already handled, but `last_seen + 1` overflowed on `u64::MAX`:
        //   a panic under overflow checks, a silent wrap to zero in release, which then
        //   read as "resume from the beginning".
        // * **Ahead of it.** A position never issued. This fell through to the filter
        //   below, matched nothing, and returned an empty replay -- so the subscriber was
        //   told it was up to date and sat looking at an empty lobby list for ever.
        if last_seen > self.browser_position() {
            return None;
        }
        let log = self.browser_log();
        let oldest = log.front().map(|(id, _)| *id)?;
        if last_seen.saturating_add(1) < oldest {
            return None;
        }
        Some(
            log.iter()
                .filter(|(id, _)| *id > last_seen)
                .cloned()
                .collect(),
        )
    }

    pub fn next_lobby_id(&self) -> u64 {
        self.lobby_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn lobby_count(&self) -> usize {
        self.lobbies.len()
    }

    /// The one predicate the envelope rules exist for. Returns the lobby the sender is
    /// in, and whether the target is in it too.
    pub fn are_co_members(&self, from: Sid, to: Sid) -> bool {
        let Some(membership) = self.members.get(&from) else {
            return false;
        };
        let Some(code) = membership.code.clone() else {
            return false;
        };
        drop(membership);
        self.lobbies
            .get(&code)
            .is_some_and(|lobby| lobby.members.contains_key(&to))
    }

    /// Collects the sids to deliver to, excluding one. The guard is released before the
    /// caller sends anything.
    pub fn peers_in(&self, code: &str, except: Sid) -> Vec<Sid> {
        match self.lobbies.get(code) {
            Some(lobby) => lobby
                .members
                .keys()
                .copied()
                .filter(|sid| *sid != except)
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn publish(&self, event: BrowserEvent) {
        let id = self.browser_seq.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut log = self.browser_log();
            if log.len() == BROWSER_LOG_LEN {
                log.pop_front();
            }
            log.push_back((id, event.clone()));
        }
        // An error here means nobody is listening, which is the normal case.
        let _ = self.browser.send((id, event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(json: serde_json::Value) -> PublicLobbyInput {
        serde_json::from_value(json).expect("the shape is optional throughout")
    }

    /// Every field of a lobby payload is whatever an unauthenticated client chose to
    /// send. These are the cases that took the Node server down before it coerced them.
    #[test]
    fn a_number_where_a_string_belongs_becomes_empty() {
        let lobby = input(serde_json::json!({ "title": 42, "host": true })).sanitise(1, 0);
        assert_eq!(
            lobby.title, "ERROR",
            "an unusable title falls back rather than panicking"
        );
        assert_eq!(lobby.host, "");
    }

    #[test]
    fn text_is_truncated_by_characters_and_not_by_bytes() {
        let lobby = input(serde_json::json!({ "title": "ü".repeat(40) })).sanitise(1, 0);
        assert_eq!(lobby.title.chars().count(), 20);
        // Truncating by bytes would split a two-byte character in half here.
        assert!(lobby.title.is_char_boundary(lobby.title.len()));
    }

    #[test]
    fn counts_are_clamped_and_truncated() {
        let lobby = input(serde_json::json!({
            "current_players": 3.7,
            "max_players": -5,
        }))
        .sanitise(1, 0);
        assert_eq!(lobby.current_players, 3);
        assert_eq!(lobby.max_players, 0);
    }

    #[test]
    fn a_string_where_a_count_belongs_becomes_zero() {
        let lobby = input(serde_json::json!({ "max_players": "15" })).sanitise(1, 0);
        assert_eq!(lobby.max_players, 0);
    }

    #[test]
    fn either_public_flag_publishes() {
        assert!(input(serde_json::json!({ "isPublic": true })).is_public());
        assert!(input(serde_json::json!({ "isPublic2": true })).is_public());
        assert!(!input(serde_json::json!({ "isPublic": "yes" })).is_public());
        assert!(!input(serde_json::json!({})).is_public());
    }

    #[test]
    fn mods_are_upper_cased() {
        assert_eq!(
            input(serde_json::json!({ "mods": "none" }))
                .sanitise(1, 0)
                .mods,
            "NONE"
        );
    }

    #[test]
    fn a_lobby_serialises_with_the_field_names_the_clients_read() {
        let lobby = input(serde_json::json!({ "isPublic": true })).sanitise(7, 123);
        let json = serde_json::to_value(&lobby).unwrap();
        for field in [
            "current_players",
            "max_players",
            "isPublic",
            "gameState",
            "stateTime",
        ] {
            assert!(
                json.get(field).is_some(),
                "{field} is missing from the wire shape"
            );
        }
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(
            crate::config::PeerConfigFile::default().resolve(
                None,
                None,
                crate::config::DEFAULT_TURN_TTL,
            ),
            None,
            "http://x".to_owned(),
        )
    }

    #[test]
    fn a_last_event_id_at_the_maximum_is_answered_rather_than_overflowing() {
        // `Last-Event-ID` is a request header, so this is whatever a client sent. It used
        // to panic under overflow checks and wrap to zero in release; and once that was
        // fixed it still returned an *empty* replay, which tells the subscriber it is up
        // to date and leaves it looking at nothing.
        let state = state();
        state.publish(BrowserEvent::RemoveLobby(1));
        assert!(
            state.replay_since(Some(u64::MAX)).is_none(),
            "a position beyond anything held has to fall back to a snapshot"
        );
    }

    #[test]
    fn a_position_ahead_of_anything_issued_also_falls_back_to_a_snapshot() {
        // Not only the maximum: any id this server has not issued yet. A subscriber that
        // sends one is out of step with us, and the only honest answer is the whole list.
        let state = state();
        state.publish(BrowserEvent::RemoveLobby(1));
        assert!(state.replay_since(Some(99)).is_none());
    }

    #[test]
    fn a_position_still_held_replays_only_what_came_after_it() {
        let state = state();
        state.publish(BrowserEvent::RemoveLobby(1));
        state.publish(BrowserEvent::RemoveLobby(2));
        let replayed = state
            .replay_since(Some(1))
            .expect("position 1 is still held");
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].0, 2);
    }

    #[test]
    fn no_position_means_a_snapshot() {
        assert!(state().replay_since(None).is_none());
    }
}
