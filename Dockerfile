# The AnotherCrewLink signalling server.
#
#     docker build -t anothercrewlink-server .
#     docker run -p 127.0.0.1:9736:9736 -e HOSTNAME=your.host.name anothercrewlink-server
#
# Publish to the host's loopback, not 0.0.0.0. TLS terminates at a reverse proxy, so
# binding the container's port to a public interface would put a plaintext WebSocket
# endpoint on the internet — the one thing this design must not do.
#
# Configuration is environment only: PORT, BIND, NAME, ADDRESS, HOSTNAME, PEER_CONFIG
# and RUST_LOG, read with `std::env::var`. There is no dotfile loader in the binary, so
# compose `environment:` or `--env-file` is the whole mechanism. The peer configuration
# is TOML; copy `config/peerConfig.example.toml` to `config/peerConfig.toml` and mount
# it read-only. Its `relay` credentials must match TURN_USER / TURN_PASSWORD in `.env`,
# which is what the coturn sidecar in docker-compose.yml reads.
#
##################################################################################
# Builder
##################################################################################
FROM rust:1.98.0-alpine3.24@sha256:3ffeca71d0e4fc30f5537f76b7243e87ac99726b6d3d66591dfc5e497078b9fc AS builder

# Pinned three ways on purpose: the tag names the toolchain, the tag also names the
# Alpine underneath it, and the digest names the exact bytes. A tag can be repointed by
# whoever owns it; a digest cannot. Both digests name a *single-architecture* manifest
# rather than a multi-arch index, which is what locks this to x86-64 -- a build on an
# arm64 workstation fails outright instead of quietly producing an image no deployment
# can run. An explicit `--platform` would do the same and buildkit warns about it, so the
# digest carries the guarantee alone.
#
# 1.98.0 is the current stable, released 2026-08-18 -- looked up rather than assumed. The
# tag is the toolchain pin, so `rust-toolchain.toml` is deliberately *not* copied in: the
# official images carry a minimal profile, and that file would make rustup download
# rustfmt and clippy that a container build has no use for. The tag and the file both say
# 1.98.0; if one of them moves, the other has to move with it.

WORKDIR /build

ENV CARGO_TERM_COLOR=never     CARGO_NET_RETRY=5

# No hardening RUSTFLAGS here, and the absence is deliberate rather than an oversight.
# The two that get reached for do nothing measurable in this image:
#
# * `relro` / `now` protect a GOT that a dynamic loader fills in. This binary is a fully
#   static musl one -- there is no loader and no lazy binding to harden.
# * `stack-protector=strong` is a `-Z` flag and needs nightly. The toolchain is pinned
#   stable, on purpose, and swapping a channel for one flag is a bad trade.
#
# `strip` is already `true` in `[profile.release]`, so setting `-C strip=symbols` here
# would be a second place to change the same thing.
#
# The hardening that does bite is further down and in compose: no userland in the runtime
# image, a non-root high UID, a read-only root filesystem, every capability dropped and
# `no-new-privileges`. Also *not* set: `panic=abort`. The server runs `CatchPanicLayer`,
# so a panic has to unwind into a 500 for the one request rather than take the process
# down and every other player's call with it.

# `cargo auditable` writes the exact dependency graph into the binary, so months later
# `cargo audit bin acl-server` can still answer "is this artefact affected" against a
# running container, without the tree that built it. Its own layer, so it is cached
# independently of this project's dependencies.
RUN cargo install cargo-auditable --locked --version 0.7.0

# Dependencies first, against a stand-in `main`. This layer is keyed on the manifest
# and the lockfile alone, so editing anything under src/ reuses the downloaded registry
# and the compiled dependency graph instead of fetching crates.io again.
#
# `--locked` fails rather than silently resolving something the lockfile does not name,
# which is the same guarantee `npm ci` gives the Node image.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo auditable build --release --locked \
    && rm -rf src \
        target/release/acl-server target/release/acl-server.d \
        target/release/deps/acl_server* \
        target/release/.fingerprint/acl-server-*

# Now the real thing. `tests/` is not copied: the wire test drives a Node
# socket.io-client and belongs to CI, not to a release image.
COPY src ./src
RUN cargo auditable build --release --locked

# The container health probe, compiled straight by rustc with no dependencies — see
# docker/healthcheck.rs for why it is not a second bin target in the crate.
COPY docker/healthcheck.rs ./docker/healthcheck.rs
RUN rustc --edition 2024 -O -C panic=abort -C strip=symbols \
        -o /build/acl-healthcheck docker/healthcheck.rs

# Both binaries are statically linked, because the host triple of the Alpine images is
# `*-unknown-linux-musl` and musl targets link `crt-static` by default. That is a
# deliberate choice and it is what makes the final stage simple: no libc to copy, no
# `ld-musl` to keep alive, no runtime dependency on the base image at all. The one cost
# is musl's allocator, which is slower than glibc's under many threads; if a busy
# deployment ever measures that as a problem the answer is an allocator crate in
# Cargo.toml, not a glibc base image that would undo everything below.

##################################################################################
# Runtime
##################################################################################
FROM alpine:3.24.1@sha256:79ff19e9084a00eece421b2523fb93e22d730e2c0e525905de047e848e56d95f

# 3.24.1 is the current stable Alpine, and the digest is the amd64 one. Almost all of
# this base is deleted three lines further down, so the version matters less here than
# in most images -- what survives is /etc and the directory skeleton. It is still
# pinned, because `addgroup` and `adduser` run before the deletion and a repointed tag
# could change what they are.

# Scratch discipline on an Alpine base: create the account, then delete the userland.
# What survives is /etc (passwd, group), the empty system directories, and whatever is
# copied in below — no shell, no busybox, no apk, no libc. `docker exec` into this
# container finds nothing to exec, and a payload that lands in it finds nothing to run.
#
# This is the last command in the image that needs a shell, and it has to be: busybox
# is unlinked halfway through it. Unlinking a running executable is fine on Linux — the
# mapping outlives the directory entry — but no later RUN could work, so there is none.
#
# The UID is fixed and high so it collides with nothing on a host that shares a
# namespace, and everything the process reads stays owned by root: the server writes
# nothing at runtime, so it has no business being able to rewrite its own binary or its
# peer configuration.
RUN set -eu; \
    addgroup -g 10001 -S acl; \
    adduser -u 10001 -S -D -H -h /nonexistent -s /sbin/nologin -G acl acl; \
    rm -rf /media /mnt /opt /srv /home /root /var/cache/apk /etc/apk /usr /sbin /bin /lib

# Standard OCI metadata, so `docker inspect` and any scanner that reads labels can say
# what this is and where it came from without a registry lookup.
LABEL org.opencontainers.image.title="AnotherCrewLink signalling server"       org.opencontainers.image.description="Socket.IO signalling relay for AnotherCrewLink proximity voice chat"       org.opencontainers.image.source="https://github.com/greluc/AnotherCrewLink-Server"       org.opencontainers.image.licenses="GPL-3.0-or-later"       org.opencontainers.image.base.name="docker.io/library/alpine:3.24.1"

COPY --from=builder /build/target/release/acl-server /app/acl-server
COPY --from=builder /build/acl-healthcheck /app/acl-healthcheck

# The example configuration ships so that an operator can `docker cp` it out of the
# image — with no shell there is no other way to read it — and so that the mount point
# exists whether or not a real config is mounted over it. With no `peerConfig.toml`
# present the server logs that fact and serves its built-in STUN default, so the image
# is runnable as-is.
COPY config/peerConfig.example.toml /app/config/peerConfig.example.toml

# BIND is 0.0.0.0 here and 127.0.0.1 everywhere else, and the difference is not a
# relaxation. A published port never reaches a process bound to the container's own
# loopback, so the default that is correct for a host install is unreachable in a
# container. The isolation the loopback default buys is provided instead by the network
# namespace plus the loopback-only port publication described at the top of this file —
# both of which live in the compose file, where an operator can see them.
ENV PORT=9736 \
    BIND=0.0.0.0 \
    PEER_CONFIG=/app/config/peerConfig.toml

# RUST_LOG is unset on purpose: main.rs already falls back to `info,acl_server=info`,
# and baking the same value here would create a second place to change it.

WORKDIR /app
USER 10001:10001
EXPOSE 9736/tcp

# `curl` is not in this image and neither is `sh`, so the probe is a binary that speaks
# HTTP/1.0 to the server's own /health and exits 0 or 1. Exec form, because there is no
# shell to parse a string form. It reads PORT and BIND from the same environment the
# server does, so the two cannot drift apart.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/app/acl-healthcheck"]

# No init shim. The server installs its own SIGTERM handler and closes the socket.io
# namespaces before axum's graceful shutdown waits on connections, which is exactly what
# PID 1 has to do; it spawns no children, so there is nothing to reap either. `docker
# stop` therefore ends in a clean shutdown rather than a ten-second timeout and SIGKILL.
ENTRYPOINT ["/app/acl-server"]
