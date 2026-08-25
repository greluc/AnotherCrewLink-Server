#!/bin/sh
# coturn, configured from the environment and pointed at the address the line actually
# has.
#
# Two jobs, and both exist because the upstream image cannot do them.
#
# **The address.** coturn has to tell every client where to send relayed media. Behind NAT
# it cannot know that address, so it is given one. On a residential line that address
# changes -- a PPPoE re-dial hands out a new one, typically once a day -- and coturn then
# names an address where nobody answers. Nothing crashes and nothing is logged: the relay
# is simply silently broken, which is the worst shape a fault can take.
#
# This is the containerised form of the host script that did the same job with a systemd
# timer, and it keeps that script's two good decisions: several sources, because one can
# be down or wrong, and a restart only when the address really changed, because a restart
# drops every allocation and with it every relayed call in progress. It differs in staying
# in the foreground -- coturn is a child of this script, so the container never restarts,
# its health never flaps, and the restart policy stays for real failures.
#
# **The arguments.** They used to live in a compose file, which meant the quadlet had to
# repeat them, and two lists of security-relevant flags that must agree is exactly the
# duplication this project keeps removing. They are here now. The quadlet does nothing but
# set environment variables, and anything appended on the command line is passed through,
# so a one-off flag needs no edit here.
#
# See deploy/coturn-dynamic-ip.md for the conditions outside the container that all of
# this assumes.

set -eu

INTERVAL="${TURN_IP_CHECK_INTERVAL:-300}"
STATIC_IP="${TURN_EXTERNAL_IP:-}"
PORT="${TURN_PORT:-3478}"
MIN_PORT="${TURN_MIN_PORT:-49160}"
MAX_PORT="${TURN_MAX_PORT:-49800}"
REALM="${TURN_REALM:-${PUBLIC_HOSTNAME:-anothercrewlink}}"
coturn_pid=""

log() { printf '%s external-ip: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }

if [ -z "${TURN_SECRET:-}" ]; then
    log "FATAL: TURN_SECRET is unset. The server derives each client's credential from it,"
    log "       so an empty one here would authenticate nobody. See .env.example."
    exit 1
fi

# Starts coturn with the external address, the settings from the environment, and
# anything the caller appended. A plain function rather than a built argument string:
# expanding a list through command substitution would split TURN_SECRET on whitespace,
# and a secret that silently loses half of itself authenticates nobody.
#
# The deny list is the reason a public TURN server is not an open proxy into the loopback
# and private ranges of the host it runs on. Removing an entry is how TURN servers end up
# in other people's incident reports.
#
# `--no-tls` and `--no-dtls` rather than pinning TLS versions. This deployment configures
# no certificate and advertises `turn:` rather than `turns:`, so the TLS and DTLS listeners
# would have nothing to serve; not opening them is one fewer port and one fewer parser
# reachable from the internet.
#
# What stood here was `--no-tlsv1 --no-tlsv1_1`, carried over from a coturn 4.7
# configuration. **`--no-tlsv1_1` does not exist in 4.17.2 and coturn refuses to start on
# it** -- found by running the container rather than by reading the diff, which is the only
# way an unknown flag ever announces itself.
run_coturn() {
    external="$1"
    shift
    turnserver \
        --external-ip="${external}" \
        --listening-port="${PORT}" \
        --min-port="${MIN_PORT}" \
        --max-port="${MAX_PORT}" \
        --realm="${REALM}" \
        --use-auth-secret \
        --static-auth-secret="${TURN_SECRET}" \
        --fingerprint \
        --no-multicast-peers \
        --denied-peer-ip=0.0.0.0-0.255.255.255 \
        --denied-peer-ip=10.0.0.0-10.255.255.255 \
        --denied-peer-ip=127.0.0.0-127.255.255.255 \
        --denied-peer-ip=169.254.0.0-169.254.255.255 \
        --denied-peer-ip=172.16.0.0-172.31.255.255 \
        --denied-peer-ip=192.168.0.0-192.168.255.255 \
        --denied-peer-ip=::1 \
        --denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff \
        --denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff \
        --no-cli \
        --no-tls \
        --no-dtls \
        --pidfile=/tmp/turnserver.pid \
        "$@" &
    coturn_pid=$!
}

# Rejects everything that cannot be a public address. Handing coturn a private or reserved
# address is exactly the silent breakage this script exists to prevent, and a discovery
# service that answers with nonsense must not be believed.
#
# 100.64.0.0/10 is in the list for a reason worth knowing: it is carrier-grade NAT. If a
# line reports one of those, the connection has no forwardable public address at all and
# this deployment cannot work -- see deploy/coturn-dynamic-ip.md.
plausible() {
    case "$1" in
        '' | *[!0-9.]*) return 1 ;;
        0.* | 10.* | 127.* | 169.254.* | 192.168.* | 255.*) return 1 ;;
        172.1[6-9].* | 172.2[0-9].* | 172.3[01].*) return 1 ;;
        100.6[4-9].* | 100.[7-9][0-9].* | 100.1[01][0-9].* | 100.12[0-7].*) return 1 ;;
    esac
    echo "$1" | grep -Eq '^([0-9]{1,3}\.){3}[0-9]{1,3}$' || return 1
    return 0
}

discover() {
    # busybox wget rather than curl: it is already in the base image, and one fewer package
    # is one fewer thing to patch in a container that faces the internet.
    for url in https://api.ipify.org https://ifconfig.me/ip https://icanhazip.com; do
        ip=$(wget -qO- -T 8 "$url" 2>/dev/null | tr -d '[:space:]') || continue
        if plausible "$ip"; then
            printf '%s' "$ip"
            return 0
        fi
    done
    return 1
}

current_ip() {
    if [ -n "$STATIC_IP" ]; then
        printf '%s' "$STATIC_IP"
        return 0
    fi
    discover
}

stop_coturn() {
    [ -n "$coturn_pid" ] || return 0
    kill -TERM "$coturn_pid" 2>/dev/null || true
    wait "$coturn_pid" 2>/dev/null || true
    coturn_pid=""
}

terminate() {
    log "shutting down"
    stop_coturn
    exit 0
}
trap terminate TERM INT

ip=$(current_ip) || {
    # Refusing to start is deliberate. Starting without an external address on a NAT'd host
    # means coturn advertises a private one, and every relayed call fails in a way that
    # looks like a client problem. With a restart policy this is retried, and the log says
    # what is wrong.
    log "FATAL: no source returned a usable public address, and TURN_EXTERNAL_IP is unset"
    exit 1
}

log "starting on port ${PORT}, relay ${MIN_PORT}-${MAX_PORT}, external ${ip}"
run_coturn "$ip" "$@"

# A fixed address needs no polling; the loop would only burn requests at three strangers'
# services for ever.
if [ -n "$STATIC_IP" ]; then
    wait "$coturn_pid"
    exit $?
fi

while :; do
    # `wait` on coturn would block until it exits, so sleep in a child and watch both.
    sleep "$INTERVAL" &
    sleep_pid=$!
    wait "$sleep_pid" 2>/dev/null || true

    if ! kill -0 "$coturn_pid" 2>/dev/null; then
        log "coturn exited on its own; letting the container follow it"
        wait "$coturn_pid" 2>/dev/null
        exit $?
    fi

    new_ip=$(discover) || {
        log "no source answered; keeping $ip"
        continue
    }
    [ "$new_ip" = "$ip" ] && continue

    # Only here, and only for a real change: this drops every allocation, so every call
    # currently going through the relay reconnects.
    log "$ip -> $new_ip, restarting coturn"
    stop_coturn
    ip="$new_ip"
    run_coturn "$ip" "$@"
done
