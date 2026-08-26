//! Configuration: the process environment, and the peer configuration file.
//!
//! There is no dotfile loader. Production supplies the environment through the quadlet's
//! `EnvironmentFile=`, and `std::env::var` reads it, which is one dependency fewer in a
//! server that is deliberately small.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

/// An ICE server as the client expects to receive it.
///
/// `urls` is a string or an array of strings in the browser API, and the client's own
/// validator accepts both, so this keeps the untagged shape rather than normalising it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Urls,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Urls {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RelaySettings {
    pub enabled: bool,
    /// Falls back to `PUBLIC_HOSTNAME`, and then to `HOSTNAME`.
    pub host: Option<String>,
    /// Falls back to `TURN_PORT`, and then to 3478.
    ///
    /// `Option` rather than a defaulted `u16` so that "the file said 3478" and "the file
    /// said nothing" are different states. They have to be: coturn's listening port comes
    /// from `TURN_PORT` in the environment, and if an absent value here silently meant
    /// 3478 then moving coturn to another port would leave every client being told the
    /// old one -- a relay nobody can reach, with nothing logged on either side.
    pub port: Option<u16>,
    pub username: String,
    pub credential: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PeerConfigFile {
    pub force_relay_only: bool,
    pub relay: RelaySettings,
    pub ice_servers: Vec<IceServer>,
}

impl Default for PeerConfigFile {
    fn default() -> Self {
        Self {
            force_relay_only: false,
            relay: RelaySettings::default(),
            ice_servers: vec![IceServer {
                urls: Urls::One("stun:stun.l.google.com:19302".to_owned()),
                username: None,
                credential: None,
            }],
        }
    }
}

/// How long an issued TURN credential stays valid, when `TURN_TTL_SECONDS` says nothing.
///
/// A day. The credential is handed out once, in `clientPeerConfig`, at the moment a client
/// connects — so the ceiling is not "how long should a secret live" but "how long might a
/// session last before ICE needs to gather again". A player who leaves the client open
/// overnight and starts a lobby in the morning must not find the relay refusing them.
pub const DEFAULT_TURN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The default TURN port, used when neither the peer config nor `TURN_PORT` says.
pub const DEFAULT_TURN_PORT: u16 = 3478;

/// What the relay advertisement takes from the process environment.
///
/// Grouped rather than passed as four positional arguments, because three of them are
/// `Option` and a call site reading `resolve(None, None, x, None)` says nothing about
/// which `None` is which.
#[derive(Debug, Clone, Copy, Default)]
pub struct RelayEnvironment<'a> {
    /// `PUBLIC_HOSTNAME`, then `HOSTNAME`. The relay's own `host` wins over both.
    ///
    /// **`HOSTNAME` alone is not safe to rely on in a container**, and this is not
    /// theoretical: podman and docker both set it to the container id, so a deployment
    /// whose environment file said `PUBLIC_HOSTNAME` advertised `turn:2cd620ec462e:3478`
    /// to every client -- a name that resolves nowhere, handed out with no error on
    /// either side. Hence the order, and hence the warning when the fallback is used.
    pub hostname: Option<&'a str>,
    /// `TURN_SECRET`. Its presence switches on per-client credentials.
    pub secret: Option<&'a str>,
    /// `TURN_PORT`, which is the port coturn is actually listening on.
    pub port: Option<u16>,
    /// `TURN_TTL_SECONDS`.
    pub ttl: Duration,
}

/// A relay that issues a fresh credential per client instead of sharing one forever.
#[derive(Debug, Clone)]
pub struct EphemeralTurn {
    pub host: String,
    pub port: u16,
    pub secret: String,
    pub ttl: Duration,
}

/// coturn's `use-auth-secret` scheme, which is the reason the relay no longer has to be
/// configured twice.
///
/// The username is an expiry timestamp and a client identifier, and the password is the
/// HMAC of that username under a secret both sides hold. coturn recomputes it on arrival, so nothing has to be
/// stored, synchronised or revoked — and this server never has to be told a password,
/// because it derives the same one coturn will.
///
/// **SHA-1 is not a choice.** It is what coturn computes, so this has to compute it too.
/// It is a MAC over a value the client is given anyway, keyed by a secret; the collision
/// weaknesses that retired SHA-1 for signatures do not apply, and HMAC-SHA1 has no
/// practical break. If it ever gets one, coturn moving is the prerequisite, not us.
#[must_use]
pub fn turn_credentials(
    secret: &str,
    ttl: Duration,
    now: SystemTime,
    client: &str,
) -> (String, String) {
    let expiry = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .saturating_add(ttl)
        .as_secs();
    // `<expiry>:<client>`, which is coturn's second accepted form. The bare `<expiry>`
    // was here first and it is the same string for everybody who connects in the same
    // second -- so a probe found two clients holding one credential, and the claim that
    // each player got their own was simply untrue. coturn reads the timestamp up to the
    // first colon and computes the HMAC over the whole username, so a suffix costs
    // nothing and makes that claim true.
    let username = format!("{expiry}:{client}");

    // `new_from_slice` only fails for key lengths HMAC cannot take, and HMAC takes every
    // length — long keys are hashed down. There is no error case to handle.
    let mut mac =
        <Hmac<Sha1>>::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(username.as_bytes());
    let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    (username, credential)
}

/// The peer configuration, ready to be handed to a client.
///
/// This used to be a finished `ClientPeerConfig` resolved once at start-up and cloned per
/// connection. It cannot be, once the credentials expire: the timestamp in the username
/// has to be minted when the client asks. Everything that does *not* change is still
/// resolved once and cloned, and the per-connection work is one HMAC over roughly ten
/// bytes — nanoseconds, against a WebSocket handshake that has just cost a round trip.
#[derive(Debug, Clone)]
pub struct PeerConfigProvider {
    base: ClientPeerConfig,
    ephemeral: Option<EphemeralTurn>,
}

impl PeerConfigProvider {
    /// The configuration for a client connecting now.
    ///
    /// `client` goes into the TURN username, so two clients never hold the same
    /// credential. The socket id is what the caller passes; anything stable and unique
    /// for the connection would do.
    #[must_use]
    pub fn issue(&self, client: &str) -> ClientPeerConfig {
        self.issue_at(SystemTime::now(), client)
    }

    /// The same, at a stated time, so the credential can be tested without waiting a day.
    #[must_use]
    pub fn issue_at(&self, now: SystemTime, client: &str) -> ClientPeerConfig {
        let Some(turn) = &self.ephemeral else {
            return self.base.clone();
        };
        let (username, credential) = turn_credentials(&turn.secret, turn.ttl, now, client);
        let mut config = self.base.clone();
        // UDP first and TCP second, for the reason the static path gives below: a `turn:`
        // URL with no transport means UDP, and the networks that need a relay most are
        // often the ones that block outbound UDP.
        for transport in ["udp", "tcp"] {
            config.ice_servers.push(IceServer {
                urls: Urls::One(format!(
                    "turn:{}:{}?transport={transport}",
                    turn.host, turn.port
                )),
                username: Some(username.clone()),
                credential: Some(credential.clone()),
            });
        }
        config
    }

    /// How many ICE servers a client is told about, for the start-up log line.
    #[must_use]
    pub fn advertised_count(&self) -> usize {
        self.base.ice_servers.len() + if self.ephemeral.is_some() { 2 } else { 0 }
    }

    #[must_use]
    pub fn force_relay_only(&self) -> bool {
        self.base.force_relay_only
    }

    #[must_use]
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral.is_some()
    }

    /// The host clients are told to reach the relay on, for the start-up log.
    #[must_use]
    pub fn relay_host(&self) -> Option<&str> {
        self.ephemeral
            .as_ref()
            .map(|turn| turn.host.as_str())
            .or_else(|| {
                self.base
                    .ice_servers
                    .iter()
                    .find_map(|server| match &server.urls {
                        Urls::One(url) if url.starts_with("turn:") => url
                            .strip_prefix("turn:")
                            .and_then(|rest| rest.split(':').next()),
                        _ => None,
                    })
            })
    }
}

/// What every client is told at connection time. The field names are the client's, not
/// this file's: `src/renderer/validateClientPeerConfig.ts` rejects anything else.
#[derive(Debug, Clone, Serialize)]
pub struct ClientPeerConfig {
    #[serde(rename = "forceRelayOnly")]
    pub force_relay_only: bool,
    #[serde(rename = "iceServers")]
    pub ice_servers: Vec<IceServer>,
}

impl PeerConfigFile {
    /// Reads the file if it is there, and falls back to the default otherwise. A broken
    /// file is a configuration mistake, not a reason to refuse to start: the server logs
    /// it and serves the default, which is what the Node version did.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no peer config file, using defaults");
                return Self::default();
            }
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "could not read the peer config");
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "peer config is not valid TOML, using defaults");
                Self::default()
            }
        }
    }

    /// Resolves everything that does not change, once, at start-up.
    ///
    /// With `turn_secret` set the relay is advertised with a credential minted per
    /// connection — see [`PeerConfigProvider`] — and `relay.username` / `relay.credential`
    /// in the file are ignored. Without it the file's static pair is used, exactly as
    /// before, because a deployment that has not moved to a shared secret must keep
    /// working across the upgrade.
    pub fn resolve(self, environment: RelayEnvironment<'_>) -> PeerConfigProvider {
        // The port coturn listens on and the port clients are told have to be the same
        // number. The file wins if it names one, because a deployment may put a proxy or a
        // different external port in front; otherwise `TURN_PORT` -- the value coturn
        // itself was started with -- is the answer, and only then the standard 3478.
        let relay_port = self
            .relay
            .port
            .or(environment.port)
            .unwrap_or(DEFAULT_TURN_PORT);
        let relay_host = self
            .relay
            .host
            .clone()
            .or_else(|| environment.hostname.map(str::to_owned));

        let mut ice_servers = self.ice_servers;
        let mut ephemeral = None;

        if self.relay.enabled {
            match relay_host {
                None => {
                    tracing::error!(
                        "relay.enabled is set but there is no relay.host and no HOSTNAME in the environment"
                    );
                }
                Some(host) if environment.secret.is_some_and(|secret| !secret.is_empty()) => {
                    // Nothing is pushed here: the two entries are minted per connection,
                    // because the username is an expiry timestamp.
                    ephemeral = Some(EphemeralTurn {
                        host,
                        port: relay_port,
                        secret: environment.secret.unwrap_or_default().to_owned(),
                        ttl: environment.ttl,
                    });
                    if !self.relay.username.is_empty() || !self.relay.credential.is_empty() {
                        tracing::warn!(
                            "TURN_SECRET is set, so relay.username and relay.credential in the peer config are ignored"
                        );
                    }
                }
                Some(host) => {
                    if self.relay.username.is_empty() || self.relay.credential.is_empty() {
                        tracing::error!(
                            "relay.enabled is set but relay.username or relay.credential is empty"
                        );
                    }
                    // Two entries, not one. A `turn:` URL with no transport parameter
                    // means UDP in WebRTC, and a client on a network that blocks
                    // outbound UDP — most schools, many offices, some mobile carriers —
                    // then cannot reach the relay at all. Those are exactly the networks
                    // that needed a relay in the first place, so advertising only UDP
                    // leaves the people this exists for with nothing.
                    //
                    // UDP first: it is what everyone who can use it should use, and ICE
                    // tries candidates in the order it is given them.
                    for transport in ["udp", "tcp"] {
                        ice_servers.push(IceServer {
                            urls: Urls::One(format!(
                                "turn:{host}:{relay_port}?transport={transport}"
                            )),
                            username: Some(self.relay.username.clone()),
                            credential: Some(self.relay.credential.clone()),
                        });
                    }
                }
            }
        }

        PeerConfigProvider {
            base: ClientPeerConfig {
                force_relay_only: self.force_relay_only,
                ice_servers,
            },
            ephemeral,
        }
    }
}

/// Everything the process needs from its environment.
#[derive(Debug, Clone)]
pub struct Settings {
    pub bind: SocketAddr,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub peer_config_path: PathBuf,
    /// Reported by `/health` and `/` so an operator can tell which address a
    /// reverse-proxied instance believes it is serving.
    pub public_address: String,
    /// The secret coturn was started with as `--static-auth-secret`.
    ///
    /// Its presence is what switches the relay from a shared password in the peer config
    /// to a credential minted per client. Environment rather than file, so it sits beside
    /// coturn's own copy in one `.env` and there is nothing to keep in step by hand.
    pub turn_secret: Option<String>,
    /// How long an issued credential is good for.
    pub turn_ttl: Duration,
    /// `TURN_PORT`: the port coturn was started on, and therefore the port clients have
    /// to be told about unless the peer config names one explicitly.
    pub turn_port: Option<u16>,
}

impl Settings {
    pub fn from_env() -> Self {
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(9736);

        // Loopback by default. TLS terminates at a reverse proxy, which is what keeps a
        // crypto stack out of this binary; binding to every interface by default would
        // quietly undo that.
        let host: IpAddr = std::env::var("BIND")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

        let peer_config_path = std::env::var("PEER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config/peerConfig.toml"));

        Self {
            bind: SocketAddr::new(host, port),
            name: std::env::var("NAME").ok().filter(|s| !s.is_empty()),
            // `PUBLIC_HOSTNAME` first. `HOSTNAME` remains a fallback for host installs
            // that have set it for years, but it cannot be the primary: podman and docker
            // set it to the container id, so in a container it is always present and
            // always wrong.
            hostname: std::env::var("PUBLIC_HOSTNAME")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    std::env::var("HOSTNAME")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .inspect(|name| {
                            tracing::warn!(
                                %name,
                                "no PUBLIC_HOSTNAME, falling back to HOSTNAME -- in a container this is the container id and clients will be told to reach a name that resolves nowhere"
                            );
                        })
                }),
            peer_config_path,
            public_address: std::env::var("ADDRESS")
                .unwrap_or_else(|_| format!("http://127.0.0.1:{port}")),
            turn_secret: std::env::var("TURN_SECRET").ok().filter(|s| !s.is_empty()),
            turn_ttl: std::env::var("TURN_TTL_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .map_or(DEFAULT_TURN_TTL, Duration::from_secs),
            turn_port: std::env::var("TURN_PORT")
                .ok()
                .and_then(|value| value.parse().ok()),
        }
    }
}

#[cfg(test)]
mod ephemeral_turn_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn ephemeral(secret: &str, ttl: Duration) -> PeerConfigProvider {
        toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            port = 3478
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            secret: Some(secret),
            ttl,
            ..Default::default()
        })
    }

    #[test]
    fn the_advertised_port_follows_the_port_coturn_was_started_on() {
        // The bug this exists for: `TURN_PORT` moves coturn, the peer config still says
        // nothing, and clients keep being told 3478. A relay nobody can reach, and neither
        // side logs anything, because both are doing exactly what they were configured to.
        let config = toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            username = "u"
            credential = "p"
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            port: Some(34780),
            ttl: DEFAULT_TURN_TTL,
            ..Default::default()
        })
        .issue("test-client");
        assert_eq!(
            config.ice_servers[1].urls,
            Urls::One("turn:turn.example.com:34780?transport=udp".to_owned())
        );
    }

    #[test]
    fn a_port_in_the_file_still_wins_over_the_environment() {
        // A deployment may put a different external port in front of coturn, and the file
        // is where that is said. Explicit beats inherited.
        let config = toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            port = 5000
            username = "u"
            credential = "p"
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            port: Some(34780),
            ttl: DEFAULT_TURN_TTL,
            ..Default::default()
        })
        .issue("test-client");
        assert_eq!(
            config.ice_servers[1].urls,
            Urls::One("turn:turn.example.com:5000?transport=udp".to_owned())
        );
    }

    #[test]
    fn with_neither_it_is_the_standard_port() {
        let config = toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            username = "u"
            credential = "p"
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            ttl: DEFAULT_TURN_TTL,
            ..Default::default()
        })
        .issue("test-client");
        assert_eq!(
            config.ice_servers[1].urls,
            Urls::One(format!(
                "turn:turn.example.com:{DEFAULT_TURN_PORT}?transport=udp"
            ))
        );
    }

    #[test]
    fn the_credential_is_the_one_coturn_will_recompute() {
        // A fixed vector, so a refactor that changes the algorithm fails here rather than
        // in a lobby. coturn's `use-auth-secret` takes the username as the message and the
        // shared secret as the key, HMAC-SHA1, base64 of the raw tag -- and the username
        // is the expiry, not the time of issue.
        //
        // The expected value is not this code's own output written down. It was computed
        // independently:
        //
        //     python -c "import hmac,hashlib,base64;         //         print(base64.b64encode(hmac.new(b's3cr3t', b'1003600', hashlib.sha1).digest()).decode())"
        //
        // which is the point of a vector: if it only ever agreed with the implementation
        // beside it, it would pass through any change to that implementation.
        let (username, credential) =
            turn_credentials("s3cr3t", Duration::from_secs(3600), at(1_000_000), "abc");
        assert_eq!(username, "1003600:abc");
        assert_eq!(credential, "mqFJ0nErRnJ740nClchV8RG42fo=");
    }

    #[test]
    fn a_later_client_gets_a_different_credential() {
        // The whole point of the scheme: nothing is shared between two players, so one
        // leaked credential expires instead of having to be rotated everywhere.
        let provider = ephemeral("s3cr3t", Duration::from_secs(3600));
        let first = provider.issue_at(at(1_000_000), "socket-a");
        let second = provider.issue_at(at(1_000_060), "socket-b");
        assert_ne!(first.ice_servers, second.ice_servers);
    }

    #[test]
    fn the_relay_is_still_advertised_over_both_transports() {
        // The static path's reason applies unchanged: a `turn:` URL with no transport
        // means UDP, and the networks that need a relay most often block outbound UDP.
        let config = ephemeral("s3cr3t", DEFAULT_TURN_TTL).issue_at(at(0), "test-client");
        let urls: Vec<&Urls> = config.ice_servers.iter().map(|s| &s.urls).collect();
        assert_eq!(
            urls,
            vec![
                &Urls::One("stun:stun.l.google.com:19302".to_owned()),
                &Urls::One("turn:turn.example.com:3478?transport=udp".to_owned()),
                &Urls::One("turn:turn.example.com:3478?transport=tcp".to_owned()),
            ]
        );
    }

    #[test]
    fn both_transports_carry_the_same_credential() {
        // One allocation, one credential. Two different ones would mean two HMACs and a
        // client that authenticates over UDP but not over TCP, which is the failure that
        // only shows up on the networks that fall back to TCP.
        let config = ephemeral("s3cr3t", DEFAULT_TURN_TTL).issue_at(at(500), "test-client");
        let turn: Vec<_> = config
            .ice_servers
            .iter()
            .filter(|server| server.username.is_some())
            .collect();
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0].username, turn[1].username);
        assert_eq!(turn[0].credential, turn[1].credential);
    }

    #[test]
    fn the_username_is_an_expiry_in_the_future_and_the_ttl_decides_how_far() {
        // coturn reads the timestamp up to the first colon and ignores the rest, so the
        // expiry has to stay the leading field however the suffix changes.
        let issued = ephemeral("s3cr3t", Duration::from_secs(60)).issue_at(at(1_000), "socket-a");
        let username = issued.ice_servers[1].username.clone().unwrap();
        let (expiry, client) = username.split_once(':').expect("expiry, then client");
        assert_eq!(expiry.parse::<u64>().unwrap(), 1_060);
        assert_eq!(client, "socket-a");
    }

    #[test]
    fn two_clients_in_the_same_second_get_different_credentials() {
        // The property the probe against the live server disproved. With the expiry alone
        // as the username, everybody who connected in the same second held one credential
        // -- and "each player gets their own" was written down in several places while
        // being false.
        let provider = ephemeral("s3cr3t", DEFAULT_TURN_TTL);
        let a = provider.issue_at(at(1_000), "socket-a");
        let b = provider.issue_at(at(1_000), "socket-b");
        assert_ne!(a.ice_servers[1].username, b.ice_servers[1].username);
        assert_ne!(a.ice_servers[1].credential, b.ice_servers[1].credential);
    }

    #[test]
    fn no_secret_means_the_static_path_and_nothing_minted() {
        // Deployments that have not moved to a shared secret must keep working across the
        // upgrade, so an absent TURN_SECRET is not an error and changes nothing.
        let provider = toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            username = "u"
            credential = "p"
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            ttl: DEFAULT_TURN_TTL,
            ..Default::default()
        });
        assert!(!provider.is_ephemeral());
        let config = provider.issue("test-client");
        assert_eq!(config.ice_servers[1].username.as_deref(), Some("u"));
        assert_eq!(config.ice_servers[1].credential.as_deref(), Some("p"));
    }

    #[test]
    fn an_empty_secret_counts_as_no_secret() {
        // `TURN_SECRET=` in a .env file is how an operator turns it off, and an empty
        // string keyed into an HMAC would otherwise be a valid, guessable secret.
        let provider = toml::from_str::<PeerConfigFile>(
            r#"
            [relay]
            enabled = true
            host = "turn.example.com"
            username = "u"
            credential = "p"
            "#,
        )
        .expect("parses")
        .resolve(RelayEnvironment {
            secret: Some(""),
            ttl: DEFAULT_TURN_TTL,
            ..Default::default()
        });
        assert!(!provider.is_ephemeral());
    }

    #[test]
    fn the_advertised_count_matches_what_is_actually_issued() {
        // The start-up log line reports this, and a number that disagrees with the config
        // it describes is worse than no number.
        let provider = ephemeral("s3cr3t", DEFAULT_TURN_TTL);
        assert_eq!(
            provider.advertised_count(),
            provider.issue("test-client").ice_servers.len()
        );
    }
}

#[cfg(test)]
mod peer_config_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// What the Node server this replaces sends today, measured from the running one at
    /// aucl.greluc.me on 2026-08-24. The Rust server has to produce the same bytes: the
    /// clients that read it are already deployed, and a field renamed or a type changed is
    /// a lobby that silently falls back to STUN only.
    const LIVE: &str = r#"{"forceRelayOnly":false,"iceServers":[{"urls":"stun:stun.l.google.com:19302"},{"urls":"turn:aucl.greluc.me:3478?transport=udp","username":"aucl","credential":"secret"},{"urls":"turn:aucl.greluc.me:3478?transport=tcp","username":"aucl","credential":"secret"}]}"#;

    fn from_toml(text: &str) -> PeerConfigFile {
        toml::from_str(text).expect("the example parses")
    }

    #[test]
    fn a_relay_is_advertised_exactly_as_the_node_server_advertises_one() {
        let config = from_toml(
            r#"
            force_relay_only = false

            [[ice_servers]]
            urls = "stun:stun.l.google.com:19302"

            [relay]
            enabled = true
            host = "aucl.greluc.me"
            port = 3478
            username = "aucl"
            credential = "secret"
            "#,
        );
        let resolved = config
            .resolve(RelayEnvironment {
                ttl: DEFAULT_TURN_TTL,
                ..Default::default()
            })
            .issue("test-client");
        let json = serde_json::to_string(&resolved).expect("serialises");
        assert_eq!(json, LIVE);
    }

    #[test]
    fn the_relay_is_appended_rather_than_replacing_the_stun_servers() {
        // A client behind a symmetric NAT needs the relay; everyone else should still
        // find the cheaper direct path first, and STUN is what finds it.
        let config = from_toml(
            r#"
            [[ice_servers]]
            urls = "stun:stun.l.google.com:19302"

            [relay]
            enabled = true
            host = "example.com"
            port = 3478
            username = "u"
            credential = "c"
            "#,
        );
        let resolved = config
            .resolve(RelayEnvironment {
                ttl: DEFAULT_TURN_TTL,
                ..Default::default()
            })
            .issue("test-client");
        assert_eq!(resolved.ice_servers.len(), 3);
        assert!(matches!(
            &resolved.ice_servers[0].urls,
            Urls::One(url) if url.starts_with("stun:")
        ));
        // UDP before TCP: ICE tries them in the order it is given, and everyone who can
        // use UDP should.
        assert!(matches!(
            &resolved.ice_servers[1].urls,
            Urls::One(url) if url == "turn:example.com:3478?transport=udp"
        ));
        assert!(matches!(
            &resolved.ice_servers[2].urls,
            Urls::One(url) if url == "turn:example.com:3478?transport=tcp"
        ));
    }

    #[test]
    fn a_disabled_relay_is_not_advertised() {
        let config = from_toml(
            r#"
            [[ice_servers]]
            urls = "stun:stun.l.google.com:19302"

            [relay]
            enabled = false
            host = "example.com"
            port = 3478
            username = "u"
            credential = "c"
            "#,
        );
        assert_eq!(
            config
                .resolve(RelayEnvironment {
                    ttl: DEFAULT_TURN_TTL,
                    ..Default::default()
                })
                .issue("test-client")
                .ice_servers
                .len(),
            1
        );
    }

    #[test]
    fn the_host_falls_back_to_the_environment() {
        // Documented behaviour, and the one a container deployment relies on.
        let config = from_toml(
            r#"
            [relay]
            enabled = true
            port = 3478
            username = "u"
            credential = "c"
            "#,
        );
        let resolved = config
            .resolve(RelayEnvironment {
                hostname: Some("from-env.example"),
                ttl: DEFAULT_TURN_TTL,
                ..Default::default()
            })
            .issue("test-client");
        assert!(resolved.ice_servers.iter().any(
            |server| matches!(&server.urls, Urls::One(url) if url == "turn:from-env.example:3478?transport=udp")
        ));
    }

    #[test]
    fn a_relay_with_nowhere_to_point_is_left_out_rather_than_advertised_broken() {
        // `enabled` with no host and no HOSTNAME. Advertising `turn::3478` would make
        // every client spend its gathering timeout on a name that cannot resolve.
        let config = from_toml(
            r#"
            [[ice_servers]]
            urls = "stun:stun.l.google.com:19302"

            [relay]
            enabled = true
            port = 3478
            username = "u"
            credential = "c"
            "#,
        );
        let resolved = config
            .resolve(RelayEnvironment {
                ttl: DEFAULT_TURN_TTL,
                ..Default::default()
            })
            .issue("test-client");
        assert_eq!(resolved.ice_servers.len(), 1, "only the STUN server");
    }

    #[test]
    fn the_example_file_parses_and_advertises_nothing_by_default() {
        // Shipping an example that does not parse is a deployment that fails at start-up,
        // and shipping one with the relay on is a server handing out empty credentials.
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/peerConfig.example.toml"),
        )
        .expect("the example is where the README says");
        let config: PeerConfigFile = toml::from_str(&text).expect("the example parses");
        assert!(!config.relay.enabled);
        assert!(!config.force_relay_only);
        let resolved = config
            .resolve(RelayEnvironment {
                hostname: Some("example.com"),
                ttl: DEFAULT_TURN_TTL,
                ..Default::default()
            })
            .issue("test-client");
        assert_eq!(resolved.ice_servers.len(), 1);
    }

    #[test]
    fn a_missing_file_serves_the_default_rather_than_refusing_to_start() {
        // A configuration mistake should not take a server that was running fine offline.
        let config = PeerConfigFile::load(std::path::Path::new("no/such/peerConfig.toml"));
        assert!(!config.relay.enabled);
        assert_eq!(config.ice_servers.len(), 1);
    }

    #[test]
    fn a_broken_file_serves_the_default_too() {
        let directory = std::env::temp_dir().join("acl-server-config-test");
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join("broken.toml");
        std::fs::write(&path, "this is not = = toml").expect("writing");
        let config = PeerConfigFile::load(&path);
        assert!(!config.relay.enabled);
        let _ = std::fs::remove_file(&path);
    }
}
