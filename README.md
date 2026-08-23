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
connections are forced through a relay, the built-in TURN server, and any external
STUN/TURN servers advertised to clients.

**Change `defaultUsername` and `defaultPassword` before exposing the integrated relay.**
The values in the example file are published in this repository, so leaving them in
place lets anyone relay traffic through your server. The server logs a warning on
startup if it finds them.

For anything beyond small private use, prefer a dedicated TURN server such as coturn
and list it under `iceServers` instead of enabling the integrated relay.

## Licence

GPL-3.0-or-later. Forked from
[BetterCrewLink-server](https://github.com/OhMyGuus/BetterCrewLink-server) by OhMyGuus,
itself a fork of [CrewLink-server](https://github.com/ottomated/CrewLink-server) by
ottomated.
