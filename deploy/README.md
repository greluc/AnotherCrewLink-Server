# Running the Rust server in production

This directory holds what is needed to run `acl-server` and its TURN relay on a Linux host
under **rootless Podman**, behind a reverse proxy. The units are quadlets, so both
containers are ordinary systemd services. Everything here assumes the two decisions the port was designed
around:

- **TLS terminates at the reverse proxy.** The server binds to the loopback interface by
  default and contains no crypto stack. It never sees a certificate and never should.
- **Configuration comes from the process environment.** There is no dotfile loader and
  no configuration-file crate. systemd's `EnvironmentFile=` supplies the environment;
  `std::env::var` reads it.

Contents:

| File | What it is |
| --- | --- |
| `quadlet/` | The two systemd units, and how to install them |
| `coturn-dynamic-ip.md` | The relay on a residential line, and what the router and the line must provide |
| `README.md` | This file: the environment, the peer configuration, and nginx |

---

## 1. Installing

See [quadlet/README.md](quadlet/README.md). In short: build the images somewhere with
memory to spare, load them on the server, drop two `.container` files into
`~/.config/containers/systemd/`, and `systemctl --user daemon-reload`.

**Do not build on the server.** A release build of this workspace with LTO wants more
memory than a small VM has, and the build tool is what falls over.

There is no compose file and no Docker. `deploy/quadlet/README.md` explains why that is a
constraint rather than a preference: rootless Docker cannot give the relay the host network
namespace it needs.

---

## 2. Environment variables

Every variable the server reads, taken from `src/config.rs` and `src/main.rs`. There are
no others.

| Variable | Default | What it does |
| --- | --- | --- |
| `PORT` | `9736` | TCP port to listen on. |
| `BIND` | `127.0.0.1` | IP address to bind. Loopback by default, deliberately — see the note below. |
| `PEER_CONFIG` | `config/peerConfig.toml` | Path to the peer configuration file. Relative paths resolve against the unit's `WorkingDirectory=`. |
| `NAME` | unset | Display name on the status page and in `/health`. An empty string counts as unset. |
| `HOSTNAME` | unset | Fallback for `relay.host` when the peer config does not set one. An empty string counts as unset. |
| `ADDRESS` | `http://127.0.0.1:$PORT` | The address reported by `/` and `/health`. Behind a proxy this is the only way the server can know its public URL, so set it. |
| `RUST_LOG` | `info,acl_server=info` | `tracing-subscriber` env-filter directives. |
| `TURN_SECRET` | unset | The secret coturn was started with as `--static-auth-secret`. Setting it switches the relay from a shared password in `peerConfig.toml` to a credential minted per client. An empty string counts as unset. |
| `TURN_PORT` | `3478` | The port coturn listens on, and therefore the port clients are told about — unless `peerConfig.toml` names one explicitly, which wins. |
| `TURN_TTL_SECONDS` | `86400` | How long an issued credential is valid. It is handed out once, when a client connects, so it has to outlive a session rather than a request. |

The relay reads more of these — `TURN_MIN_PORT`, `TURN_MAX_PORT`, `TURN_REALM`,
`TURN_EXTERNAL_IP`, `TURN_IP_CHECK_INTERVAL` — from the same file. They are documented in
`.env.example` and in [coturn-dynamic-ip.md](coturn-dynamic-ip.md).

Three things worth knowing before they surprise someone:

- **`PORT` and `BIND` fall back silently on a parse failure.** `PORT=nine` gives you
  9736, not an error. If the server is not on the port you expected, check for a typo
  before checking anything else.
- **`HOSTNAME` is not inherited.** An interactive shell sets `HOSTNAME`, but systemd does
  not set it for services. If the relay advertisement depends on it, put it in the
  environment file explicitly.
- **`ADDRESS` is cosmetic but load-bearing for support.** It is what an operator reads off
  `/health` to tell which instance they are looking at, and what the status page tells a
  player to paste into Settings → Server. A wrong value here produces bug reports that
  are impossible to reproduce.

Do not set `BIND=0.0.0.0` unless you have decided, deliberately, to put an untrusted
network in front of a server that has no TLS. The loopback default is what keeps the
crypto stack out of this binary; changing it quietly undoes that.

A working `~/.config/acl/acl.env`:

```sh
# Listening socket. Loopback only; nginx is what the internet talks to.
PORT=9736
BIND=127.0.0.1

# What the status page and /health report as this server's public address.
ADDRESS=https://voice.example.com

# Shown on the status page.
NAME=example.com voice

# The path inside the container; the quadlet mounts ~/.config/acl/config there.
PEER_CONFIG=/app/config/peerConfig.toml

# Public hostname, used as the TURN relay host when peerConfig.toml omits relay.host.
HOSTNAME=turn.example.com

RUST_LOG=info,acl_server=info
```

Both containers read the same file through the quadlets' `EnvironmentFile=`, which is why
there is one of it and not two.

### Peer configuration

`config/peerConfig.example.toml` documents itself. It is TOML rather than YAML because
the maintained Rust YAML crates depend on an archived machine translation of libyaml,
and this file is a handful of URL/username/credential fields.

A missing file is not an error — the server logs it and serves the built-in default (one
Google STUN server, no relay). A *malformed* file is also not an error: it is logged and
the default is served. That is deliberate and matches the Node server, but it means a
broken peer config presents as "TURN stopped working" rather than as a failed start.
Check the journal after editing it:

```bash
journalctl --user -u acl-server -n 20 | grep -i 'peer config'
```

The file is read once, at start-up. Editing it needs a `systemctl --user restart acl-server`.

---

## 3. nginx

The proxy terminates TLS and forwards to loopback. Three things have to be right: the
WebSocket upgrade, the read timeout, and not buffering the lobby stream.

The `map` goes at `http { }` level, once per nginx instance:

```nginx
# Translates the presence of an Upgrade header into the right Connection header.
# A bare `proxy_set_header Connection "upgrade"` breaks every non-WebSocket request
# through the same server block.
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

# Switching between the Node server and the Rust server is a change to this one line.
upstream acl_backend {
    server 127.0.0.1:9736;
}
```

Then the server block:

```nginx
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name voice.example.com;

    ssl_certificate     /etc/letsencrypt/live/voice.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/voice.example.com/privkey.pem;

    # Nothing here takes a meaningful request body. The server caps its own at 16 KB.
    client_max_body_size 32k;

    # --- socket.io: an HTTP request that becomes a WebSocket ----------------------
    location /socket.io/ {
        proxy_pass http://acl_backend;

        # Upgrade requires HTTP/1.1 to the upstream. HTTP/2 on the client side is fine
        # and unrelated.
        proxy_http_version 1.1;
        proxy_set_header Upgrade    $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # A WebSocket carrying a quiet lobby can be idle for minutes. nginx's 60 s
        # default closes it and the client reconnects, which players see as everyone
        # briefly disappearing from the list. Engine.IO's own ping keeps it alive well
        # inside an hour.
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }

    # --- the lobby stream: a long-lived SSE response ------------------------------
    # This is the one that fails silently if you skip it. Exact-match location, so it
    # wins over the prefix location below.
    location = /lobbies/stream {
        proxy_pass http://acl_backend;
        proxy_http_version 1.1;

        # SSE is not an upgrade. An empty Connection header stops nginx forwarding the
        # hop-by-hop header it received.
        proxy_set_header Connection "";
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Without this nginx holds each event in a buffer and the browser sees nothing
        # until the buffer fills — the stream appears to work and is minutes stale.
        proxy_buffering off;
        proxy_cache off;
        gzip off;

        # The response never ends. The server sends a keep-alive comment every 20 s, so
        # even nginx's 60 s default would survive; raise it anyway, because the default
        # is what makes this endpoint fragile the moment the heartbeat is retuned.
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }

    # --- everything else: /, /health, /lobbies, /lobbies/{id}/code ----------------
    location / {
        proxy_pass http://acl_backend;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # A lobby code is the credential that gates entry to a game. The server sends
        # `Cache-Control: no-store` on /lobbies/{id}/code; this makes sure nothing in
        # front of it decides otherwise.
        proxy_cache off;
    }
}
```

Two notes on what nginx is *not* doing here:

- There is no CORS handling in the proxy. The server puts CORS on the HTTP endpoints a
  browser fetches with XHR and deliberately puts none on the socket.io route: a WebSocket
  upgrade is not a CORS request. Adding an `Origin` allow-list at the proxy would restrict
  only browsers, and the only browser that matters is the OBS overlay page.
- `client_max_body_size` bounds HTTP request bodies. It has no effect at all on WebSocket
  frames after the upgrade. See §5.

---

## 4. Switching from the Node server to this one, and back

Both servers default to port 9736, so run them on different ports and let the proxy
decide which one is live. The switch is then one line and a reload, and the rollback is
the same line and another reload — no rebuild, no reinstall, nothing to redeploy.

The two servers agree about which messages they refuse: the signal envelope rules are on
in the Rust server from its first commit, and the H3 hardening release switched them on
in the Node server. That is what makes this a deployment change rather than a behaviour
change.

**Prepare.** Give the Rust server its own port and start it alongside the Node server:

```sh
# /etc/acl-server/acl-server.env
PORT=9737
```

```bash
sudo systemctl restart acl-server
curl -s http://127.0.0.1:9737/health
```

You should get JSON with `uptime`, `connectionCount`, `lobbiesCount`, `address`, `name`
and a `counters` object. Nothing is proxied to it yet.

**Switch.**

```bash
# upstream acl_backend { server 127.0.0.1:9737; }
sudo nginx -t && sudo systemctl reload nginx
```

Existing WebSockets stay on the old upstream until they reconnect; new connections go to
the Rust server immediately. Watch both:

```bash
journalctl -u acl-server -f
watch -n5 'curl -s https://voice.example.com/health'
```

`connectionCount` climbing on the Rust server and flat on the Node one means the switch
took. Then check the four counters on `/health` — `droppedFullBuffer`, `refusedSignals`,
`refusedOversize`, `refusedMalformed`. A rising `refusedSignals` against a client fleet
that is on 1.0.5 or newer is the signal that something is wrong; the other three should
be at or near zero.

**Roll back.** Put `9736` back in the `upstream` block and reload nginx. The Node server
has been running the whole time, so this takes as long as a reload does. Leave both
running for at least one full session's worth of play before stopping either.

**Finish.** Once the Rust server has carried real traffic long enough to trust:

```bash
sudo systemctl disable --now anothercrewlink-server   # whatever the Node unit is called
```

Move the Rust server to 9736 and change the `upstream` line back only if you want the
port numbers tidy. It buys nothing and it costs you the ability to roll back in one
reload, so consider leaving it on 9737.

---

## 5. Accepted risk: the inbound WebSocket cap is 64 MiB, not the one you set

Recorded here because it cannot be configured away, and an operator who does not know
about it will look for a setting that does not exist.

**socketioxide never applies `max_payload` to the WebSocket transport.** The value the
server sets governs two things — the size of outbound `emit()` payloads, and the number
advertised in the Engine.IO handshake — and a hostile client simply ignores the
advertisement. socketioxide enforces the cap on the polling transport, which this server
does not offer, so the transport that is actually in use never sees it.

What does bound an inbound message there is tungstenite's default: **64 MiB per message,
16 MiB per frame**. So this is a cap in the wrong place rather than no cap at all. The
exposure is a hostile client forcing a 64 MiB allocation where the configured limit says
64 KB — a factor of a thousand — and the per-event check below refuses the payload only
after the frame has been decoded, which means it does not prevent that allocation.

**This cannot be fixed at the reverse proxy.** Neither nginx nor Caddy has a directive
that bounds a WebSocket frame after the upgrade completes; `client_max_body_size` and its
equivalents apply to HTTP request bodies and stop mattering the moment the connection is
upgraded. There is no configuration line that closes this. Fixing it properly is an
upstream change to socketioxide or a fork.

What actually bounds the exposure today:

1. **The per-event size check in `src/socket.rs`.** Every relayed payload is measured
   before it is forwarded, and an oversize one is refused and counted as
   `refusedOversize` on `/health`. This is the real limit on what one client can make the
   server send to other clients.
2. **`MemoryMax=512M` in the unit.** This is the backstop for the part the size check
   cannot cover: memory allocated while receiving a frame, before any of the server's own
   code sees it. With the limit in place a client that inflates the process gets the
   process killed and restarted in about two seconds; without it, it gets the host's OOM
   killer to choose a victim, and the victim is not necessarily this service.
3. **`TasksMax=` and `LimitNOFILE=`**, which bound the number of connections that can be
   doing this at once.

That is mitigation, not a fix, and it is written down as mitigation on purpose. If the
upstream issue is resolved, the per-event check stays and this section goes.

---

## 6. Verifying a deployment

```bash
# The service is up and the sandbox applied.
systemctl status acl-server
systemd-analyze security acl-server

# The server answers on loopback.
curl -s http://127.0.0.1:9736/health

# The proxy forwards the plain HTTP routes.
curl -s https://voice.example.com/health
curl -s https://voice.example.com/lobbies

# The stream stays open and heartbeats. This should print an event immediately, then a
# keep-alive comment every 20 seconds, and should not close on its own.
curl -N -s https://voice.example.com/lobbies/stream

# The WebSocket upgrade is proxied. A 101 here is the whole test.
curl -s -i -o /dev/null -w '%{http_code}\n' \
     -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
     -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
     'https://voice.example.com/socket.io/?EIO=4&transport=websocket'
```

If the last one returns 400 rather than 101, the `map` block is missing or the request
did not reach the `/socket.io/` location. If the stream test closes after exactly 60
seconds, `proxy_read_timeout` was not raised on that location.

The end-to-end check that matters is the one the plan names: a stock 1.0.2 Electron
client connects, joins a lobby, exchanges signalling, and the lobby browser populates —
with no client change whatsoever.
