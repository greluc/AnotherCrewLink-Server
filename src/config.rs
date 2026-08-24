//! Configuration: the process environment, and the peer configuration file.
//!
//! There is no dotfile loader. Production supplies the environment through systemd's
//! `EnvironmentFile=` or docker's `--env-file`, and `std::env::var` reads it, which is
//! one dependency fewer in a server that is deliberately small.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RelaySettings {
    pub enabled: bool,
    /// Falls back to the `HOSTNAME` environment variable when absent.
    pub host: Option<String>,
    pub port: u16,
    pub username: String,
    pub credential: String,
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: None,
            port: 3478,
            username: String::new(),
            credential: String::new(),
        }
    }
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

    /// Resolves the relay advertisement once, at start-up, so the per-connection path
    /// does no work beyond cloning a finished value.
    pub fn resolve(self, hostname: Option<&str>) -> ClientPeerConfig {
        let relay_host = self
            .relay
            .host
            .clone()
            .or_else(|| hostname.map(str::to_owned));

        let mut ice_servers = self.ice_servers;

        if self.relay.enabled {
            match relay_host {
                None => {
                    tracing::error!(
                        "relay.enabled is set but there is no relay.host and no HOSTNAME in the environment"
                    );
                }
                Some(host) => {
                    if self.relay.username.is_empty() || self.relay.credential.is_empty() {
                        tracing::error!(
                            "relay.enabled is set but relay.username or relay.credential is empty"
                        );
                    }
                    ice_servers.push(IceServer {
                        urls: Urls::One(format!("turn:{host}:{}", self.relay.port)),
                        username: Some(self.relay.username.clone()),
                        credential: Some(self.relay.credential.clone()),
                    });
                }
            }
        }

        ClientPeerConfig {
            force_relay_only: self.force_relay_only,
            ice_servers,
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
            hostname: std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()),
            peer_config_path,
            public_address: std::env::var("ADDRESS")
                .unwrap_or_else(|_| format!("http://127.0.0.1:{port}")),
        }
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
    const LIVE: &str = r#"{"forceRelayOnly":false,"iceServers":[{"urls":"stun:stun.l.google.com:19302"},{"urls":"turn:aucl.greluc.me:3478","username":"aucl","credential":"secret"}]}"#;

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
        let resolved = config.resolve(None);
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
        let resolved = config.resolve(None);
        assert_eq!(resolved.ice_servers.len(), 2);
        assert!(matches!(
            &resolved.ice_servers[0].urls,
            Urls::One(url) if url.starts_with("stun:")
        ));
        assert!(matches!(
            &resolved.ice_servers[1].urls,
            Urls::One(url) if url == "turn:example.com:3478"
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
        assert_eq!(config.resolve(None).ice_servers.len(), 1);
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
        let resolved = config.resolve(Some("from-env.example"));
        assert!(resolved.ice_servers.iter().any(
            |server| matches!(&server.urls, Urls::One(url) if url == "turn:from-env.example:3478")
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
        let resolved = config.resolve(None);
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
        let resolved = config.resolve(Some("example.com"));
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
