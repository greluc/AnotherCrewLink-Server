# The AnotherCrewLink signalling server, Rust edition.
#
# The build context is `server-rs/`, not the repository root:
#
#     docker build -f server-rs/Dockerfile -t anothercrewlink-server:rs server-rs
#
# Nothing here touches the Node server, its Dockerfile at the repository root, or the
# CI job that builds it. The two images are built from two different contexts and can
# exist side by side for as long as the switch-over takes.
#
# ---------------------------------------------------------------------------------
# Taking the Node server's place in docker-compose.yml
# ---------------------------------------------------------------------------------
# The compose file is not edited by this change. An operator switching a deployment
# over changes four things in the `server` service and nothing else:
#
#     server:
#       build: ./server-rs                      # was: build: .
#       restart: unless-stopped
#       ports:
#         - '127.0.0.1:9736:9736'               # was: '9736:9736'
#       environment:
#         HOSTNAME: ${PUBLIC_HOSTNAME}
#         ADDRESS: https://${PUBLIC_HOSTNAME}   # new; what /health and / report
#       volumes:
#         - ./server-rs/config:/app/config:ro   # was: ./config:/app/config:ro
#
# and writes `server-rs/config/peerConfig.toml` — TOML, not the Node server's
# `config/peerConfig.yml`. Copy `peerConfig.example.toml` next to it and edit; the
# `relay` credentials there must still match TURN_USER / TURN_PASSWORD in `.env`, which
# is what the coturn sidecar reads. The coturn service is untouched by any of this.
#
# The port publication changes because TLS terminates at a reverse proxy: publishing on
# 0.0.0.0 puts a plaintext WebSocket endpoint on the public internet, which is the one
# thing this design must not do. Publish to the host's loopback and let the proxy in
# front of it hold the certificate.
#
# Everything else is environment: PORT, BIND, NAME, ADDRESS, HOSTNAME, PEER_CONFIG and
# RUST_LOG, read with `std::env::var` — there is no dotfile loader in the binary, so
# compose `environment:` or `--env-file` is the whole configuration mechanism.

##################################################################################
# Builder
##################################################################################
FROM rust:1.98-alpine AS builder

# The image tag is the toolchain pin, so `rust-toolchain.toml` is deliberately *not*
# copied in: the official images carry a minimal profile, and that file would make
# rustup download rustfmt and clippy that a container build has no use for. The tag and
# the file both say 1.98.0; if one of them moves, the other has to move with it.

WORKDIR /build

ENV CARGO_TERM_COLOR=never \
    CARGO_NET_RETRY=5

# Dependencies first, against a stand-in `main`. This layer is keyed on the manifest
# and the lockfile alone, so editing anything under src/ reuses the downloaded registry
# and the compiled dependency graph instead of fetching crates.io again.
#
# `--locked` fails rather than silently resolving something the lockfile does not name,
# which is the same guarantee `npm ci` gives the Node image.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src \
        target/release/aucl-server target/release/aucl-server.d \
        target/release/deps/aucl_server* \
        target/release/.fingerprint/aucl-server-*

# Now the real thing. `tests/` is not copied: the wire test drives a Node
# socket.io-client and belongs to CI, not to a release image.
COPY src ./src
RUN cargo build --release --locked

# The container health probe, compiled straight by rustc with no dependencies — see
# docker/healthcheck.rs for why it is not a second bin target in the crate.
COPY docker/healthcheck.rs ./docker/healthcheck.rs
RUN rustc --edition 2024 -O -C panic=abort -C strip=symbols \
        -o /build/aucl-healthcheck docker/healthcheck.rs

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
FROM alpine:3.22

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
    addgroup -g 10001 -S aucl; \
    adduser -u 10001 -S -D -H -h /nonexistent -s /sbin/nologin -G aucl aucl; \
    rm -rf /media /mnt /opt /srv /home /root /var/cache/apk /etc/apk /usr /sbin /bin /lib

COPY --from=builder /build/target/release/aucl-server /app/aucl-server
COPY --from=builder /build/aucl-healthcheck /app/aucl-healthcheck

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

# RUST_LOG is unset on purpose: main.rs already falls back to `info,aucl_server=info`,
# and baking the same value here would create a second place to change it.

WORKDIR /app
USER 10001:10001
EXPOSE 9736/tcp

# `curl` is not in this image and neither is `sh`, so the probe is a binary that speaks
# HTTP/1.0 to the server's own /health and exits 0 or 1. Exec form, because there is no
# shell to parse a string form. It reads PORT and BIND from the same environment the
# server does, so the two cannot drift apart.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/app/aucl-healthcheck"]

# No init shim. The server installs its own SIGTERM handler and closes the socket.io
# namespaces before axum's graceful shutdown waits on connections, which is exactly what
# PID 1 has to do; it spawns no children, so there is nothing to reap either. `docker
# stop` therefore ends in a clean shutdown rather than a ten-second timeout and SIGKILL.
ENTRYPOINT ["/app/aucl-server"]
