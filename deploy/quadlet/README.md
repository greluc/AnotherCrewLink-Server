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

Nothing. They are published to GitHub Container Registry by
`.github/workflows/publish.yml`, and the quadlets pull them:

```
ghcr.io/greluc/anothercrewlink-server:latest
ghcr.io/greluc/anothercrewlink-coturn:latest
```

Every build also publishes a `:sha-<commit>` tag, so any image is addressable by the
commit that produced it and a rollback is a one-line edit rather than an archaeology
exercise. Releases add `:<tag>` and move `:latest`.

**Do not build on a small server.** A release build of this workspace with LTO wants more
memory than a 2 GB VM comfortably has, and Buildah is what falls over rather than the
compiler. That is why the registry exists.

### One manual step, once

**Packages on ghcr.io are private by default**, and a private package cannot be pulled by
an unauthenticated host. After the first successful publish, open the package on GitHub —
your profile → Packages → `anothercrewlink-server` → Package settings — and set its
visibility to public. Same for `anothercrewlink-coturn`.

Leave them private if you prefer, and give the server a pull secret instead:

```bash
podman login ghcr.io -u YOUR_GITHUB_USER   # a PAT with read:packages as the password
```

`podman login` writes to `${XDG_RUNTIME_DIR}/containers/auth.json`, which does not survive
a reboot for a user without lingering — so if you go this route, enable lingering first
(below) and check that the units still start after a reboot.

### Checking the published pair yourself

The deployment verification runs against whatever images you point it at, so it can be
aimed at the registry rather than at a local build:

```bash
ACL_IMAGE_SERVER=ghcr.io/greluc/anothercrewlink-server:edge ACL_IMAGE_COTURN=ghcr.io/greluc/anothercrewlink-coturn:edge tests/deployment.sh
```

That is a different question from "does this tree build something that works", and neither
answers the other. CI runs it weekly against the registry, without logging in, so a package
that stops being publicly pullable is noticed before a deployment discovers it.

### Verifying what you pulled

Each image carries a build provenance attestation naming the workflow and the commit that
produced it:

```bash
gh attestation verify oci://ghcr.io/greluc/anothercrewlink-server:latest \
  --owner greluc
```

### Building it yourself anyway

```bash
podman build -t anothercrewlink-server:local -f Containerfile .
podman build -t anothercrewlink-coturn:local -f containers/coturn/Containerfile containers/coturn
```

Then point the `Image=` lines at the `:local` tags.

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

## If the first start times out

`acl-server.container` sets `Notify=healthy`, so systemd holds the unit in "activating"
until the container reports healthy rather than merely running. That is the nicer
behaviour — `systemctl start` returning means it is actually serving — and it is the one
line here that was never exercised under real Podman, because the machine it was written
on has none.

The image does declare the health check it waits on (`/app/acl-healthcheck`, 10 s start
period, verified on the built image), and the container answers `/health` under exactly
this unit's hardening. So the ingredients are right. But if `systemctl --user start
acl-server` sits for ninety seconds and then reports a failure while `podman ps` shows a
container that is up and answering, that line is the suspect: delete it, reload, and the
unit becomes started-when-running like any other.

```bash
podman healthcheck run acl-server    # what systemd is waiting for
podman logs acl-server
```

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
podman pull ghcr.io/greluc/anothercrewlink-server:latest
podman pull ghcr.io/greluc/anothercrewlink-coturn:latest
systemctl --user restart acl-server acl-coturn
```

To roll back, point the `Image=` line at the `:sha-<commit>` tag of the build that worked,
`systemctl --user daemon-reload`, and restart. The old image is still in the registry and
still on the host unless it was pruned.

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
