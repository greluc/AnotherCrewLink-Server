# AnotherCrewLink Server

Voice relay and signalling server for [AnotherCrewLink](https://github.com/greluc/AnotherCrewLink).

It does three things: it routes WebRTC signalling between players in the same lobby,
it hands clients an ICE configuration, and it keeps the public lobby list. Voice
itself is peer to peer and never passes through this server unless a TURN relay is in
play.

> **Compatibility:** this runs socket.io 4. Clients built on socket.io 2, including
> the original BetterCrewLink client, cannot connect, and the two protocols are not
> interoperable in either direction.

There are two servers in this repository. The Node one in `src` is the one in
production, and it is what the rest of this file is about until the Rust section at
the end. The Rust one in `server-rs` speaks the same protocol and is not in production
yet.

## Running it

```bash
npm ci
npm run build
node dist/index.js
```

Or with Docker:

```bash
docker build -t anothercrewlink-server .
docker run -p 9736:9736 -e HOSTNAME=your.host.name anothercrewlink-server
```

### Environment

| Variable | Meaning |
| --- | --- |
| `PORT` | Listening port. Defaults to 9736, or 443 when HTTPS is on. |
| `HOSTNAME` | Public hostname. Required when the integrated TURN relay is enabled. |
| `HTTPS` | Set to enable HTTPS; needs `SSL_CERT_PATH` and `SSL_KEY_PATH`. |

### Peer configuration

Copy `config/peerConfig.example.yml` to `config/peerConfig.yml`. It controls whether
connections are forced through a relay, which TURN relay clients are told about, and
any extra STUN/TURN servers.

## TURN relay

Players behind a symmetric NAT cannot connect to each other directly, so a TURN relay
is what makes those pairs work. This server does not embed one: the relay it used to
ship, node-turn, has been unmaintained since 2022.

Use [coturn](https://github.com/coturn/coturn) instead. `docker-compose.yml` starts it
alongside this server:

```bash
cp .env.example .env          # set PUBLIC_HOSTNAME, PUBLIC_IP and TURN credentials
cp config/peerConfig.example.yml config/peerConfig.yml
# put the same credentials under `relay` in peerConfig.yml, and set enabled: true
docker compose up -d
```

coturn needs UDP 3478 and the relay port range (49152-65535 by default) reachable from
the internet. Generate your own credentials; anything committed to a repository can be
used by anyone to relay traffic at your expense.

Without a relay the server still works, and most players will still connect directly.

## The Rust server

`server-rs` is phase 0 of the port described in the client repository's
`docs/rust-port`. It is the same protocol, not a new one: it serves socket.io 4, the
same eleven events and the same peer configuration, so every shipping client connects
to it with no client change at all. That is what phase 0 is for — proving the
toolchain, the CI and the release story on the smallest piece of the system before
anything else is committed to.

It is not in production. The Node server is, and it stays that way until the Rust one
has run against real clients and the port's decision point has been answered.

### Building and running it

```bash
cd server-rs
cargo build --release
target/release/aucl-server
```

Copy `config/peerConfig.example.toml` to `config/peerConfig.toml` first if you want
anything other than the default STUN server. It is TOML rather than YAML because the
maintained YAML crates for Rust depend on an archived machine translation of libyaml,
and the file is a handful of url/username/credential fields.

| Variable | Meaning |
| --- | --- |
| `PORT` | Listening port. Defaults to 9736. |
| `BIND` | Interface to bind. Defaults to 127.0.0.1. TLS terminates at a reverse proxy, so there is no HTTPS switch and no certificate paths here. |
| `HOSTNAME` | Public hostname. Used as the relay host when `peerConfig.toml` does not name one. |
| `PEER_CONFIG` | Path to the peer configuration. Defaults to `config/peerConfig.toml`. |
| `ADDRESS` | The address `/` and `/health` report, so an operator can tell which address a reverse-proxied instance believes it is serving. |
| `NAME` | Server name shown on the status page and in `/health`. |
| `RUST_LOG` | Tracing filter. Defaults to `info`. |

There is no dotfile loader. Configuration comes from the process environment — systemd's
`EnvironmentFile=` or docker's `--env-file`.

### Tests

```bash
cd server-rs
cargo test
```

The integration test in `tests/wire.rs` starts the server as a child process and drives
it with the reference `socket.io-client` from Node, so the wire format is checked
against the implementation the shipping clients actually use rather than against
another copy of our own assumptions. It needs `node` on the PATH and `npm ci` run in
the repository root. Without either it skips loudly, with a line saying why, rather
than passing quietly having checked less than it appears to.

### What behaves differently

Three things, all deliberate.

**WebSocket is the only transport, so the polling handshake is refused.** Both shipping
clients already connect with `transports: ['websocket']`, so nothing in the field
regresses, and removing polling removes the advisory it carried
([GHSA-r635-g3xr-vw7x](https://github.com/advisories/GHSA-r635-g3xr-vw7x)) along with
base64 binary framing and the probe-and-upgrade handshake. A client that does not ask
for the WebSocket transport — `socket.io-client` tries polling first by default — never
gets past the handshake.

**A signal is relayed only to a socket in the sender's own lobby.** Never to a room
name, never back to the sender, and never above 64 KB. The Node server relayed to
whatever target a sender named, which let anyone who knew a six-character lobby code
address any socket on the server and read every player's live coordinates. The cost is
that the OBS overlay feed and the mobile relay are refused as every client up to and
including 1.0.3 addresses them — both send a signal to a room name, `<code>_mobile`,
which is exactly the shape this server no longer relays. Voice is unaffected. Every
refusal is counted and the counters are on `/health`.

**The host of a lobby is the first socket to claim it**, and it holds until that socket
leaves. A later socket claiming to be host is refused rather than taking over the
host's authority over lobby settings.

### Two new endpoints

`/`, `/health` and `/lobbies` behave as they do on the Node server. Alongside them:

| Endpoint | What it does |
| --- | --- |
| `GET /lobbies/{id}/code` | The code for a public lobby, with `Cache-Control: no-store` — a lobby code is the credential that gates entry to a game. 404 if the lobby is unknown, 410 if it is no longer public. |
| `GET /lobbies/stream` | The lobby list as server-sent events: the whole list once, then updates as they happen. A keep-alive comment every 20 seconds, because nginx cuts an idle upstream at 60 by default and a silently frozen lobby list is worse than an empty one. Send `Last-Event-ID` to resume. |

## Licence

GPL-3.0-or-later. Forked from
[BetterCrewLink-server](https://github.com/OhMyGuus/BetterCrewLink-server) by OhMyGuus,
itself a fork of [CrewLink-server](https://github.com/ottomated/CrewLink-server) by
ottomated.
