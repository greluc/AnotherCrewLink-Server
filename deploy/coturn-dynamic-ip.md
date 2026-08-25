# coturn on a residential line

The relay runs on a connection whose public address changes. This is what that costs, how
the container handles it, and — the part that is not ours to handle — what has to be true
outside the container for any of it to work.

## Why an address has to be configured at all

coturn tells every client which address to send relayed media to. Behind NAT it cannot
know that address: it sees the private one on its own interface. So it is told, with
`--external-ip`, and if what it is told is stale it hands clients an address where nobody
answers.

**That failure is silent.** No service stops, nothing is logged, the container stays
healthy, and `/health` on the signalling server is green because the signalling server is
fine. What happens is that the players who need the relay — the ones who already could not
connect directly — hear nothing, while everyone else is unaffected. It reads as "some
people have problems", not as an outage.

## What the container does

`containers/coturn/entrypoint.sh` discovers the address, starts coturn with it, and rechecks
every `TURN_IP_CHECK_INTERVAL` seconds (300 by default). It restarts coturn **only when
the address actually changed**.

Two properties are carried over from the systemd-timer script this replaces, because both
were right:

- **Several discovery sources.** api.ipify.org, ifconfig.me, icanhazip.com; the first
  plausible answer wins. One can be down, and one can be wrong.
- **A restart only on a real change.** A restart drops every allocation, and with it every
  call currently going through the relay.

One thing is done differently. The timer restarted a systemd unit; here coturn is a child
of the entrypoint, so the *container* never restarts. Its health never flaps, and Docker's
restart policy stays reserved for real failures.

Set `TURN_EXTERNAL_IP` to a literal address and discovery is skipped entirely — no
polling, no requests to three strangers' services, no restarts.

## Conditions outside the container

None of these is optional, and none can be fixed from inside the image.

### 1. The line must have a real public address

If the ISP puts the connection behind **carrier-grade NAT** — an address in
`100.64.0.0/10` — there is nothing to forward to, and TURN cannot be hosted on this line
at all. No configuration changes that. The entrypoint rejects those addresses rather than
starting with one, so the failure is a refusal at boot with a message in the log, not a
relay that quietly does nothing.

The same check rejects `10/8`, `172.16/12`, `192.168/16`, `127/8`, `169.254/16`, `0/8` and
`255/8`: a discovery service that answers with one of those must not be believed.

### 2. The router must forward the ports, one to one

| What | Protocol | Why |
| --- | --- | --- |
| `TURN_PORT` (3478 by default) | UDP **and** TCP | Clients that cannot use UDP fall back to TCP, and those are exactly the clients that needed a relay |
| `TURN_MIN_PORT`–`TURN_MAX_PORT` (49160–49800 by default) | UDP | One port per active allocation |

**One to one, with no port translation.** coturn tells the client which port it allocated.
If the router hands out a different external port, the client sends media to a port
nothing is listening on. This is the most common way a TURN deployment behind a home
router is broken, and it looks exactly like a client bug.

The range is far narrower than coturn's own default of 49152–65535 — sixteen thousand
ports, which is miserable to enter into a router and, under rootless Docker, is per-port
work in rootlesskit that simply does not come up. 641 ports is 641 simultaneous relayed
allocations, which a proximity-chat server for a handful of lobbies will not reach, and it
is still one rule on a router rather than a thousand.

### 3. DynDNS has to track, with a short TTL

Two different things follow the line, by two different mechanisms:

- **Clients** are given `turn:PUBLIC_HOSTNAME:PORT` and resolve it themselves. That name
  is DynDNS's job.
- **coturn** is given the address, by the entrypoint above.

A DynDNS record with a long TTL leaves clients resolving the old address for as long as
the TTL says, however quickly coturn is corrected. Sixty seconds or less.

### 4. The host firewall must allow the same ports

Forwarding on the router puts the packets on the host's interface; a firewall there can
still drop them. With `network_mode: host` the container does no filtering of its own.

### 5. Players on the same LAN need NAT hairpinning

A client inside the same network resolves `PUBLIC_HOSTNAME` to the public address and
sends there. Routers without hairpinning (NAT loopback) drop that. Those players lose the
relay and fall back to a direct connection, which on one LAN usually works anyway — so it
is a degradation rather than an outage, but worth knowing before diagnosing it as
something else.

### 6. The daily re-dial will drop calls in progress

Most residential lines are re-dialled every 24 hours and get a new address. The entrypoint
then restarts coturn, which drops every allocation. Relayed calls reconnect; direct ones
are untouched. If the line lets you choose the hour, choose one nobody plays in.

### 7. Upstream bandwidth is the ceiling

Every relayed stream crosses the line twice, in and out again. Residential upstream is the
constraint here, not CPU.

## Rootless Docker and Podman

> **Corrected 2026-08-25.** An earlier version of this section said the TURN port might
> need `CAP_NET_BIND_SERVICE` or a lowered `net.ipv4.ip_unprivileged_port_start`. That was
> wrong. Privileged ports are 0-1023; TURN's 3478 and TURNS' 5349 are both above that and
> need no privilege at all. The `cap_add` the compose file used to carry has gone with it.

`network_mode: host` is what keeps this simple, and the two rootless runtimes differ so
much here that the choice of runtime decides how much work this is.

**Podman rootless is the easy case, and it is the recommended one.** `--network=host`
gives the container the *real* host network namespace. coturn binds the host's interfaces
directly, so there is nothing to publish, no relay range to forward through a userspace
proxy, and no address rewriting: coturn sees each client's actual source address, which is
what its allocations and its rate accounting are built on. Nothing needs elevating,
because nothing here binds a privileged port.

Podman's quadlets also fit a systemd-managed host better than a compose file does: the
container becomes an ordinary unit, with the same restart and ordering semantics as
everything else on the box.

**Rootless Docker is the hard case, and `network_mode: host` does not work there.** It
does not give the container the host's namespace; it gives it RootlessKit's, which nothing
outside can reach, and `-p` cannot be combined with host networking to compensate. The
setup has to go bridged, which brings two costs:

- The relay range has to be published one-to-one, and every published port is work for
  RootlessKit's forwarder.
- **The default port driver rewrites the source address.** coturn then sees every client
  arriving from the same address, which flattens its per-client accounting and is
  untested territory for a relay. Docker documents setting the RootlessKit port driver to
  `slirp4netns` to preserve source addresses; that fixes the correctness problem and
  costs throughput.

Bridged, publish the range with matching host and container numbers:

```yaml
ports:
  - '${TURN_PORT:-3478}:${TURN_PORT:-3478}/udp'
  - '${TURN_PORT:-3478}:${TURN_PORT:-3478}/tcp'
  - '${TURN_MIN_PORT:-49160}-${TURN_MAX_PORT:-49800}:${TURN_MIN_PORT:-49160}-${TURN_MAX_PORT:-49800}/udp'
```

and keep the range small. Podman 5 defaults to pasta, which handles this far better than
slirp4netns did, but better is not free.

## Why coturn is not inside the server image

It was considered and rejected. The reasons are worth keeping, because the question will
come up again.

**The server image has no userland.** It creates its account and then deletes `/bin`,
`/usr`, `/lib` and the package manager; no shell, no busybox and no libc survive. coturn
is a dynamically linked C daemon needing musl, OpenSSL and libevent, so merging would
restore a full Alpine userland into the image that terminates untrusted WebSocket traffic.
That is the largest single de-hardening available here.

**They want opposite network modes.** The signalling server must be published to the
host's loopback only, because TLS terminates at a reverse proxy and a plaintext WebSocket
must never reach the internet. The relay needs host networking. One container is one
network namespace, so merging means choosing, and either choice breaks one of them.

**One process, one PID 1.** The server image deliberately has no init shim: the server
handles SIGTERM, closes the socket.io namespaces before axum stops accepting, and reaps
nothing because it spawns nothing. Two daemons need a supervisor, restart semantics, and a
health check that can say which half died.

**Separate advisory streams.** coturn is a large C codebase with a real CVE history.
Merged, every coturn advisory forces a rebuild and redeploy of the signalling server too.

What was actually painful is gone, and not by merging the images. Configuring the relay
twice — once in `.env` for coturn, once in `config/peerConfig.toml` for the server, with
credentials that had to match — is replaced by one `TURN_SECRET`: coturn takes it as
`--static-auth-secret`, and the server mints a time-limited credential per client from the
same value. One variable, one file, and no shared password handed to every player for
ever.
