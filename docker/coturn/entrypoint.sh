#!/bin/sh
# Keeps coturn's --external-ip on the address the line actually has.
#
# coturn has to tell every client which address to send relayed media to. Behind NAT it
# cannot know that address itself, so it is given one. On a residential line that address
# changes -- a PPPoE re-dial hands out a new one, typically once a day -- and coturn then
# names an address where nobody answers. Nothing crashes and nothing is logged: the relay
# is simply silently broken, which is the worst shape a fault can take.
#
# This is the containerised form of the host script that did the same job with a systemd
# timer, and it keeps that script's two good decisions: several sources, because one can
# be down or wrong, and a restart only when the address really changed, because a restart
# drops every allocation and with it every relayed call in progress.
#
# What it does differently is stay in the foreground. The timer version restarted a
# systemd unit; here coturn is a child of this script, so the container itself never
# restarts, its health never flaps, and Docker's restart policy stays reserved for real
# failures.

set -eu

INTERVAL="${TURN_IP_CHECK_INTERVAL:-300}"
STATIC_IP="${TURN_EXTERNAL_IP:-}"
coturn_pid=""

log() { printf '%s external-ip: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }

# Rejects everything that cannot be a public address. Handing coturn a private or
# reserved address is exactly the silent breakage this script exists to prevent, and a
# discovery service that answers with nonsense must not be believed.
#
# 100.64.0.0/10 is in the list for a reason worth knowing: it is carrier-grade NAT. If a
# line reports one of those, the connection has no forwardable public address at all and
# this deployment cannot work -- see deploy/coturn-dynamic-ip.md.
plausible() {
    case "$1" in
        '' | *[!0-9.]* ) return 1 ;;
        0.*|10.*|127.*|169.254.*|192.168.*|255.*) return 1 ;;
        172.1[6-9].*|172.2[0-9].*|172.3[01].*) return 1 ;;
        100.6[4-9].*|100.[7-9][0-9].*|100.1[01][0-9].*|100.12[0-7].*) return 1 ;;
    esac
    # Four dotted parts, each of them a number busybox can compare.
    echo "$1" | grep -Eq '^([0-9]{1,3}\.){3}[0-9]{1,3}$' || return 1
    return 0
}

discover() {
    # busybox wget rather than curl: it is already in the base image, and one fewer
    # package is one fewer thing to patch in a container that faces the internet.
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
    # Refusing to start is deliberate. Starting without an external address on a NAT'd
    # host means coturn advertises a private one, and every relayed call fails in a way
    # that looks like a client problem. With `restart: unless-stopped` this is retried,
    # and the log says what is wrong.
    log "FATAL: no source returned a usable public address, and TURN_EXTERNAL_IP is unset"
    exit 1
}
log "starting with $ip"
turnserver --external-ip="$ip" "$@" &
coturn_pid=$!

# A fixed address needs no polling; the loop would only burn requests at three strangers'
# services forever.
if [ -n "$STATIC_IP" ]; then
    wait "$coturn_pid"
    exit $?
fi

while :; do
    # `wait` would block until coturn exits, so sleep in a child and watch both.
    sleep "$INTERVAL" &
    sleep_pid=$!
    wait "$sleep_pid" 2>/dev/null || true

    if ! kill -0 "$coturn_pid" 2>/dev/null; then
        log "coturn exited on its own; letting the container follow it"
        wait "$coturn_pid" 2>/dev/null
        exit $?
    fi

    new_ip=$(discover) || { log "no source answered; keeping $ip"; continue; }
    [ "$new_ip" = "$ip" ] && continue

    # Only here, and only for a real change: this drops every allocation, so every call
    # currently going through the relay reconnects.
    log "$ip -> $new_ip, restarting coturn"
    stop_coturn
    ip="$new_ip"
    turnserver --external-ip="$ip" "$@" &
    coturn_pid=$!
done
