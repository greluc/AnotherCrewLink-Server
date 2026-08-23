# AnotherCrewLink Server

Voice relay and signalling server for [AnotherCrewLink](https://github.com/greluc/AnotherCrewLink).

It does three things: it routes WebRTC signalling between players in the same lobby,
it hands clients an ICE configuration, and it keeps the public lobby list. Voice
itself is peer to peer and never passes through this server unless a TURN relay is in
play.

> **Compatibility:** this runs socket.io 4. Clients built on socket.io 2, including
> the original BetterCrewLink client, cannot connect, and the two protocols are not
> interoperable in either direction.

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

## Licence

GPL-3.0-or-later. Forked from
[BetterCrewLink-server](https://github.com/OhMyGuus/BetterCrewLink-server) by OhMyGuus,
itself a fork of [CrewLink-server](https://github.com/ottomated/CrewLink-server) by
ottomated.
