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
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// A token bucket, refilled continuously.
///
/// Per socket and per class of event, not per server: the thing being limited is one
/// client shouting, and a global limit would let one client silence everybody else.
///
/// `allow` takes the time rather than reading the clock, so the refill can be tested
/// without sleeping. That is the only reason this is a struct and not four lines inline.
#[derive(Debug, Clone)]
pub struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    #[must_use]
    pub fn new(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    /// Spends a token if there is one, refilling first.
    pub fn allow(&mut self, rate_per_second: f64, burst: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate_per_second).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The limits, per socket.
///
/// The numbers are set from what the shipped client actually does, with room above it,
/// because a limit that clips legitimate traffic is worse than none: it produces a bug
/// nobody can reproduce.
///
/// * `VAD` fires on a speech transition. Somebody talking in bursts produces a few a
///   second; twenty a second with a burst of forty is far above any human.
/// * `lobby` is sent when the lobby's state changes -- players joining, the game
///   starting. Two a second with a burst of ten covers a lobby filling up quickly.
/// * `signal` is ICE candidates and SDP, which arrive in a clump while a peer connects
///   and then stop. Fifty a second with a burst of a hundred covers a full mesh forming.
/// * `join` is once per lobby change. Two a second is already generous.
///
/// Over the limit the event is dropped and counted, not answered with a disconnect: a
/// client that stutters past a burst must not lose its call over it.
pub const VAD_RATE: (f64, f64) = (20.0, 40.0);
pub const LOBBY_RATE: (f64, f64) = (2.0, 10.0);
pub const SIGNAL_RATE: (f64, f64) = (50.0, 100.0);
pub const JOIN_RATE: (f64, f64) = (2.0, 5.0);

#[derive(Debug, Clone)]
pub struct Limits {
    pub vad: Bucket,
    pub lobby: Bucket,
    pub signal: Bucket,
    pub join: Bucket,
}

impl Limits {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            vad: Bucket::new(VAD_RATE.1, now),
            lobby: Bucket::new(LOBBY_RATE.1, now),
            signal: Bucket::new(SIGNAL_RATE.1, now),
            join: Bucket::new(JOIN_RATE.1, now),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

/// What a socket is currently part of.
#[derive(Debug, Clone, Default)]
pub struct Membership {
    /// `Arc<str>` rather than `String`: `are_co_members` runs on every relayed signal and
    /// has to let go of this map's guard before it touches `lobbies`, or two paths taking
    /// the two maps in opposite orders could deadlock. It therefore takes a copy, and a
    /// refcount bump is a cheaper copy than an allocation.
    pub code: Option<Arc<str>>,
    pub client: Option<Client>,
    pub watching_lobbies: bool,
    /// Per-socket rate limits. See [`Limits`].
    pub limits: Limits,
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
    /// Events dropped because the sender was over its rate limit.
    pub refused_rate_limited: AtomicU64,
    /// Lobby-stream subscribers turned away because the server was already at its cap.
    pub refused_subscribers: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "droppedFullBuffer": self.dropped_full_buffer.load(Ordering::Relaxed),
            "refusedSignals": self.refused_signals.load(Ordering::Relaxed),
            "refusedOversize": self.refused_oversize.load(Ordering::Relaxed),
            "refusedMalformed": self.refused_malformed.load(Ordering::Relaxed),
            "refusedRateLimited": self.refused_rate_limited.load(Ordering::Relaxed),
            "refusedSubscribers": self.refused_subscribers.load(Ordering::Relaxed),
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

/// One subscriber's slot, released on drop.
pub struct StreamSlot {
    state: Arc<AppState>,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.state
            .stream_subscribers
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// How much of the stream is kept for subscribers that reconnect. Twenty seconds of a
/// busy server, which is the window a dropped connection realistically returns inside.
const BROWSER_LOG_LEN: usize = 256;

/// How many lobby-stream subscribers are served at once.
///
/// `/lobbies/stream` needs no credential, and each subscriber costs a task and a
/// broadcast receiver for as long as it stays. Without a ceiling that is an unauthenticated
/// way to make the server allocate until it stops. Two hundred and fifty-six is far more
/// browsers than this has ever had open.
pub const MAX_STREAM_SUBSCRIBERS: usize = 256;

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
    /// Randomly seeded per process, so one issued id says nothing about the next.
    lobby_id_hasher: std::collections::hash_map::RandomState,
    pub connections: AtomicI64,
    /// Live `/lobbies/stream` subscribers, capped at [`MAX_STREAM_SUBSCRIBERS`].
    pub stream_subscribers: AtomicUsize,
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
            lobby_id_hasher: std::collections::hash_map::RandomState::new(),
            connections: AtomicI64::new(0),
            stream_subscribers: AtomicUsize::new(0),
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

    /// An unguessable lobby id, and one a browser can still hold exactly.
    ///
    /// It used to be `lobby_seq.fetch_add(1)`. Sequential ids make `/lobbies/{id}/code`
    /// and the `join_lobby` event walkable: count from zero and collect the join code of
    /// every public lobby on the server. Those codes are meant to be discoverable -- that
    /// is what the lobby browser is -- but discoverable one at a time through a list is
    /// not the same as harvestable in a loop.
    ///
    /// **Masked to 53 bits, and that is not arbitrary.** Both clients type this field as
    /// a JSON number, and `src/common/PublicLobby.ts` declares `id: number` -- a double.
    /// Above `Number.MAX_SAFE_INTEGER` a client would round the id, send a different one
    /// back in `join_lobby`, and be told the lobby does not exist. Nothing would log an
    /// error; the lobby would simply be unjoinable for everyone using the browser.
    /// 2^53 still leaves nine quadrillion values, which no one is counting through.
    ///
    /// The counter stays, hashed rather than exposed, so two ids issued in the same
    /// process are never equal however the hasher behaves.
    pub fn next_lobby_id(&self) -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let n = self.lobby_seq.fetch_add(1, Ordering::Relaxed);
        let mut hasher = self.lobby_id_hasher.build_hasher();
        hasher.write_u64(n);
        hasher.finish() & ((1u64 << 53) - 1)
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
        // A refcount bump rather than an allocation. The guard still has to be dropped
        // before `lobbies` is touched: taking the two maps in opposite orders anywhere
        // would be a deadlock, and this is the path that runs on every signal.
        let Some(code) = membership.code.clone() else {
            return false;
        };
        drop(membership);
        self.lobbies
            .get(code.as_ref())
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

    /// Takes a subscriber slot, or `None` when the server is already at its cap.
    ///
    /// The returned guard releases the slot when it is dropped, which is what makes this
    /// safe against every way a stream can end: the client going away, the reverse proxy
    /// timing out, a panic in the stream body. A hand-written decrement at the end of the
    /// handler would be missed by all three.
    pub fn take_stream_slot(self: &Arc<Self>) -> Option<StreamSlot> {
        let taken =
            self.stream_subscribers
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    (current < MAX_STREAM_SUBSCRIBERS).then_some(current + 1)
                });
        match taken {
            Ok(_) => Some(StreamSlot {
                state: Arc::clone(self),
            }),
            Err(_) => {
                self.counters
                    .refused_subscribers
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
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
    use std::time::Duration;

    fn state() -> AppState {
        AppState::new(
            crate::config::PeerConfigFile::default()
                .resolve(crate::config::RelayEnvironment::default()),
            None,
            "http://x".to_owned(),
        )
    }

    #[test]
    fn a_bucket_allows_its_burst_and_then_refuses() {
        let start = Instant::now();
        let mut bucket = Bucket::new(3.0, start);
        for i in 0..3 {
            assert!(bucket.allow(1.0, 3.0, start), "token {i} should be there");
        }
        assert!(
            !bucket.allow(1.0, 3.0, start),
            "the fourth in the same instant has nothing left to spend"
        );
    }

    #[test]
    fn a_bucket_refills_at_its_rate() {
        // The clock is passed in rather than read, which is the whole reason this is a
        // struct: a test for a refill that had to sleep would be a test nobody runs.
        let start = Instant::now();
        let mut bucket = Bucket::new(1.0, start);
        assert!(bucket.allow(2.0, 1.0, start));
        assert!(!bucket.allow(2.0, 1.0, start));
        // Half a second at two per second is exactly one token.
        let later = start + Duration::from_millis(500);
        assert!(bucket.allow(2.0, 1.0, later));
    }

    #[test]
    fn a_bucket_does_not_bank_more_than_its_burst() {
        // An idle client must not accumulate an hour of tokens and spend them at once,
        // which would make the limit useless against exactly the sender it is for.
        let start = Instant::now();
        let mut bucket = Bucket::new(2.0, start);
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..2 {
            assert!(bucket.allow(1.0, 2.0, much_later));
        }
        assert!(!bucket.allow(1.0, 2.0, much_later));
    }

    #[test]
    fn a_lobby_id_stays_inside_what_a_browser_can_hold_exactly() {
        // Both clients declare `id: number`. Above 2^53 a client rounds it, sends a
        // different id back in `join_lobby`, and is told the lobby does not exist --
        // with nothing logged anywhere, because from the server's side it simply asked
        // about an id that was never issued.
        let state = state();
        for _ in 0..1000 {
            let id = state.next_lobby_id();
            assert!(
                id < (1u64 << 53),
                "{id} is above Number.MAX_SAFE_INTEGER and a browser cannot hold it"
            );
        }
    }

    #[test]
    fn lobby_ids_are_not_a_sequence_anybody_can_walk() {
        // The property that matters is that knowing one id does not give you the next.
        // Sequential ids made every public lobby's join code harvestable in a loop
        // through `/lobbies/{id}/code`.
        let state = state();
        let ids: Vec<u64> = (0..64).map(|_| state.next_lobby_id()).collect();
        assert!(
            ids.windows(2).filter(|w| w[1] == w[0] + 1).count() < 2,
            "ids look consecutive: {ids:?}"
        );
        let unique: std::collections::HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "an id was issued twice");
    }

    #[test]
    fn the_stream_cap_holds_and_a_finished_subscriber_gives_its_slot_back() {
        let state = Arc::new(state());
        let held: Vec<_> = (0..MAX_STREAM_SUBSCRIBERS)
            .map(|i| {
                state
                    .take_stream_slot()
                    .unwrap_or_else(|| panic!("slot {i} should be available"))
            })
            .collect();
        assert!(
            state.take_stream_slot().is_none(),
            "the cap has to actually stop the next one"
        );
        assert_eq!(
            state.counters.refused_subscribers.load(Ordering::Relaxed),
            1
        );

        drop(held);
        assert!(
            state.take_stream_slot().is_some(),
            "dropping a subscriber has to release its slot, or the server fills up for good"
        );
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
