#!/usr/bin/env bash
# Brings the two images up the way the quadlets do, drives them with real clients, and
# ends by asking coturn to accept a credential the server minted.
#
# That last step is the one this exists for. Everything else about the shared-secret
# scheme can be unit-tested; whether coturn computes the same HMAC over the same username
# cannot, because the answer lives in coturn.
#
#     tests/deployment.sh                 # podman, the deployment runtime
#     ACL_RUNTIME=docker tests/deployment.sh
#
# By default it builds the images from this checkout. Point it at published ones instead
# to check what is actually deployable rather than what this tree can produce:
#
#     ACL_IMAGE_SERVER=ghcr.io/greluc/anothercrewlink-server:edge #     ACL_IMAGE_COTURN=ghcr.io/greluc/anothercrewlink-coturn:edge #     tests/deployment.sh
#
# Those are two different questions. A tree that builds a working pair says nothing about
# whether the pair in the registry is the same pair, or is public, or exists.
#
# **What this does not cover, and it has already misled someone.** It writes its own
# `peerConfig.toml` and its own environment, so it exercises the two images against each
# other and never the configuration a deployment actually ships. A server delivered with
# `[relay] enabled = false`, or with the relay host resolving to a container id, passes
# here and fails for every player -- both of which happened. For a deployment's own
# configuration the tool is `tests/live-probe.mjs`, which asks a running server what it
# tells clients.
#
# Uses a bridge network with a fixed subnet rather than host networking, because coturn's
# --external-ip has to be known before it starts and a pinned address is the only way to
# know it. Host networking is what the quadlet uses in production and is checked there;
# what is checked here is the pair, not the namespace.

set -euo pipefail

RUNTIME="${ACL_RUNTIME:-podman}"
NET=acl-deploy-test
SUBNET=10.89.7.0/24
COTURN_IP=10.89.7.10
SERVER_IP=10.89.7.11
TURN_PORT=3478
SECRET="deployment-test-$$"
SERVER_IMAGE="${ACL_IMAGE_SERVER:-}"
COTURN_IMAGE="${ACL_IMAGE_COTURN:-}"

command -v "$RUNTIME" >/dev/null || { echo "no $RUNTIME on PATH"; exit 1; }
if [ "$RUNTIME" != podman ]; then
    # Worth saying out loud, because it has already produced a green local run against a
    # build CI then rejected: `.containerignore` is read by podman and not by docker, so
    # under any other runtime the build context is unfiltered and an error in the
    # allow-list cannot show up here.
    echo "note: running with $RUNTIME, so .containerignore does not apply to the build"
fi
command -v node >/dev/null || { echo "no node on PATH"; exit 1; }

cleanup() {
    $RUNTIME rm -f acl-t-server acl-t-coturn >/dev/null 2>&1 || true
    $RUNTIME network rm -f "$NET" >/dev/null 2>&1 || true
    rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

# A path the container runtime accepts as a bind source. On Linux that is the path as
# written; under Git Bash on Windows the runtime is a Windows process and needs a Windows
# path, which is what cygpath produces. Without this the mount silently does nothing, the
# server serves its built-in default, and the result looks exactly like a broken relay
# configuration rather than like a broken test.
mount_path() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}

WORK=$(mktemp -d)
mkdir -p "$WORK/config"
# relay.host is deliberately absent: the server has to take it from HOSTNAME, and the port
# from TURN_PORT. If either plumbing breaks, the URL below comes out wrong and the
# allocation at the end fails -- which is the point of not writing them down here.
cat > "$WORK/config/peerConfig.toml" <<TOML
force_relay_only = false

[[ice_servers]]
urls = "stun:stun.l.google.com:19302"

[relay]
enabled = true
TOML

if [ -n "$SERVER_IMAGE" ] && [ -n "$COTURN_IMAGE" ]; then
    echo "== pulling =="
    echo "  server: $SERVER_IMAGE"
    echo "  coturn: $COTURN_IMAGE"
    $RUNTIME pull -q "$SERVER_IMAGE" >/dev/null
    $RUNTIME pull -q "$COTURN_IMAGE" >/dev/null
else
    echo "== building =="
    SERVER_IMAGE=anothercrewlink-server:test
    COTURN_IMAGE=anothercrewlink-coturn:test
    # `--format docker` under podman: OCI has no health check field, so the default would
    # build an image whose HEALTHCHECK line had been dropped. Docker writes schema 2 and
    # takes no such flag.
    fmt=""
    [ "$RUNTIME" = podman ] && fmt="--format docker"
    # shellcheck disable=SC2086
    $RUNTIME build $fmt -q -t "$SERVER_IMAGE" -f Containerfile . >/dev/null
    # shellcheck disable=SC2086
    $RUNTIME build $fmt -q -t "$COTURN_IMAGE" -f containers/coturn/Containerfile containers/coturn >/dev/null
fi

echo "== network =="
$RUNTIME network rm -f "$NET" >/dev/null 2>&1 || true
$RUNTIME network create --subnet "$SUBNET" "$NET" >/dev/null

echo "== coturn =="
$RUNTIME run -d --name acl-t-coturn --network "$NET" --ip "$COTURN_IP" \
    --read-only --tmpfs /tmp:rw,noexec,nosuid,nodev,size=1m \
    --cap-drop=ALL --cap-add=NET_BIND_SERVICE --security-opt=no-new-privileges:true \
    -e TURN_SECRET="$SECRET" \
    -e TURN_EXTERNAL_IP="$COTURN_IP" \
    -e TURN_PORT="$TURN_PORT" \
    -e TURN_REALM=deployment.test \
    "$COTURN_IMAGE" \
    --allowed-peer-ip="10.89.7.0-10.89.7.255" >/dev/null

echo "== server =="
$RUNTIME run -d --name acl-t-server --network "$NET" --ip "$SERVER_IP" \
    --read-only --cap-drop=ALL --security-opt=no-new-privileges:true \
    -e TURN_SECRET="$SECRET" \
    -e HOSTNAME="$COTURN_IP" \
    -e TURN_PORT="$TURN_PORT" \
    -e BIND=0.0.0.0 \
    -p 127.0.0.1:19736:9736 \
    -v "$(mount_path "$WORK")/config:/app/config:ro" \
    "$SERVER_IMAGE" >/dev/null

echo "== waiting =="
for _ in $(seq 1 40); do
    curl -fsS http://127.0.0.1:19736/health >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS http://127.0.0.1:19736/health >/dev/null || { echo "server never became healthy"; $RUNTIME logs acl-t-server; exit 1; }
# coturn gets its own wait, and the check is written the awkward way on purpose.
#
# `$RUNTIME logs C | grep -q PATTERN` is the obvious form and it is a trap under
# `set -o pipefail`: `grep -q` exits the moment it matches, `logs` is then killed by
# SIGPIPE, and the pipeline reports failure for a pattern that was *found*. That produced
# a CI failure whose own error handler printed a log containing the very line it had just
# said was missing. It does not reproduce under Docker, which proxies `logs` through a
# daemon and swallows the signal -- so the local run stayed green and only podman showed
# it.
#
# Capturing to a file removes the pipe, and with it the whole question.
coturn_ready() {
    $RUNTIME logs acl-t-coturn > "$WORK/coturn.log" 2>&1 || true
    grep -q "Black listing" "$WORK/coturn.log"
}

for _ in $(seq 1 40); do
    if coturn_ready; then break; fi
    sleep 1
done
if ! coturn_ready; then
    echo "coturn did not reach its peer deny list within 40s"
    echo "  container state: $($RUNTIME inspect acl-t-coturn --format '{{.State.Status}} exit={{.State.ExitCode}}' 2>&1)"
    echo "  captured $(wc -c < "$WORK/coturn.log") bytes of log:"
    cat "$WORK/coturn.log"
    exit 1
fi
echo
echo "== clients =="
ISSUED=$(ACL_URL=http://127.0.0.1:19736 node tests/deployment.mjs)

echo
echo "== the credential the server issued, against coturn =="
USERNAME=$(printf '%s' "$ISSUED" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).username))')
CREDENTIAL=$(printf '%s' "$ISSUED" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).credential))')
URLS=$(printf '%s' "$ISSUED" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).urls))')
echo "  advertised: $URLS"
echo "  username:   $USERNAME"

# coturn's own client, so the thing doing the asking did not come from this project.
#
# `-y` relays to the client's own address, which makes this a real allocation and a real
# channel bind rather than only a STUN binding -- the credential has to survive all the
# way to an allocation for that to work.
echo
run_uclient() {
    $RUNTIME run --rm --network "$NET" --entrypoint turnutils_uclient \
        "$COTURN_IMAGE" \
        -u "$1" -w "$2" -p "$TURN_PORT" -n 2 -y "$COTURN_IP" 2>&1
}

# Relaying is the discriminator, not a status code. coturn answers a bad credential with
# 401, but its own client retries and then gives up with "Cannot complete Allocation", so
# grepping for the code is brittle. What cannot be faked is traffic: the good credential
# has to move bytes through the relay, and the bad one must not.
relayed() { grep -qiE "start_mclient|total transmit time" "$1"; }

run_uclient "$USERNAME" "$CREDENTIAL" > "$WORK/good.log" 2>&1 || true
if relayed "$WORK/good.log"; then
    echo "  ok   coturn accepted the credential the server derived, and relayed with it"
else
    echo "  FAIL coturn did not relay with the credential the server derived"
    tail -15 "$WORK/good.log"
    exit 1
fi

# The negative control. Without it the check above only says "something happened"; with it,
# the difference between a credential coturn accepts and one it does not is visible, which
# is the only thing that makes the first result mean anything.
run_uclient "$USERNAME" "definitely-not-the-derived-credential" > "$WORK/bad.log" 2>&1 || true
if relayed "$WORK/bad.log"; then
    echo "  FAIL a wrong credential relayed too -- the positive result proves nothing"
    tail -15 "$WORK/bad.log"
    exit 1
fi
echo "  ok   and relays nothing for a wrong one, so the first result is not a no-op"

echo
echo "deployment verification passed"
