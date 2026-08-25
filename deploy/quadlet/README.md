# Deploying with Podman

Two containers, two systemd units, one environment file. Rootless throughout — nothing
here needs root, and the reverse proxy in front of the server is what holds the
privileged ports.

There is no compose file and no Docker. See "Why Podman and not Docker" at the end for
the reason, which is not a preference.

## What goes where

| | |
| --- | --- |
| `~/.config/containers/systemd/acl-server.container` | from this directory |
| `~/.config/containers/systemd/acl-coturn.container` | from this directory |
| `~/.config/acl/acl.env` | from `.env.example`, edited |
| `~/.config/acl/config/peerConfig.toml` | from `config/peerConfig.example.toml`, edited |

## Getting the images onto the host

**Do not build on a small server.** A release build of this workspace with LTO wants more
memory than a 2 GB VM comfortably has, and BuildKit or Buildah will be the thing that
falls over. Build where there is room and move the result:

```bash
# on a machine with memory to spare
podman build -t anothercrewlink-server:latest -f Containerfile .
podman build -t anothercrewlink-coturn:latest -f containers/coturn/Containerfile containers/coturn

podman save anothercrewlink-server:latest anothercrewlink-coturn:latest \
  | zstd -T0 > acl-images.tar.zst
```

```bash
# on the server
zstd -d < acl-images.tar.zst | podman load
```

A registry works just as well and is nicer for rollback, because the previous tag is still
there. Either way the server needs no compiler, no BuildKit and no swap trick.

## First run

```bash
loginctl enable-linger "$USER"
```

Without lingering the units stop when the last session for that user ends, and start again
only at the next login — which on a headless box means never.

```bash
mkdir -p ~/.config/containers/systemd ~/.config/acl/config
cp deploy/quadlet/*.container       ~/.config/containers/systemd/
cp .env.example                     ~/.config/acl/acl.env
cp config/peerConfig.example.toml   ~/.config/acl/config/peerConfig.toml
$EDITOR ~/.config/acl/acl.env
```

`TURN_SECRET` is the one value that must be set. Generate it, do not invent it:

```bash
openssl rand -base64 32
```

Then:

```bash
systemctl --user daemon-reload
systemctl --user start acl-server acl-coturn
```

`daemon-reload` is what turns the `.container` files into units; there is nothing to
enable, because `WantedBy=default.target` in the file does that when the generator runs.

## Checking it

```bash
systemctl --user status acl-server acl-coturn
journalctl --user -u acl-server -f
curl -fsS http://127.0.0.1:9736/health | jq
```

`/health` reports the counters — what the server has refused or dropped since it started.
`refusedRateLimited` and `refusedSubscribers` climbing steadily is worth looking at;
zero is the normal reading.

For the relay, the log line to look for on start-up names the address it will hand out:

```
external-ip: starting on port 3478, relay 49160-49800, external 203.0.113.10
```

If that address is wrong, every relayed call fails and nothing else reports a thing. See
[coturn-dynamic-ip.md](../coturn-dynamic-ip.md), which also lists what the router, the
line and DNS have to provide.

## Updating

```bash
# load the new images, then
systemctl --user restart acl-server acl-coturn
```

Restarting coturn drops every allocation, so every call going through the relay
reconnects. Restarting the signalling server disconnects every client, which they handle,
but a lobby mid-game will notice. Neither is a reason not to update; both are a reason to
pick the hour.

## Why Podman and not Docker

Not a preference. Rootless Docker cannot run this relay.

`--network=host` under rootless Podman is the real host network namespace, so coturn binds
the host's interfaces and sees each client's actual source address. Under rootless Docker
it is RootlessKit's namespace instead, which nothing outside can reach, and `-p` cannot be
combined with host networking to compensate. The fallback is publishing the whole relay
range one-to-one through a forwarder that, by default, rewrites the source address — so
coturn would see every client arriving from the same place.

Podman also fits a systemd host better: a quadlet is an ordinary unit, with the same
ordering, restart and journal behaviour as everything else on the box, rather than a
second supervisor with its own opinions.
