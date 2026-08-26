// Does a real client work against a real server?
//
// Points socket.io-client — the same library the Electron client uses — at a running
// deployment and walks the path a player's client walks: connect, be told how to reach
// the relay, join, learn who else is there, exchange a signal, leave.
//
// **Deliberately gentle, because this is aimed at production.** It joins a lobby code no
// Among Us game can produce, never emits `lobby` (so nothing appears in anyone's lobby
// browser), sends one signal rather than a stream, and disconnects. Two sockets for a few
// seconds, and nothing left behind.
//
//     node tests/live-probe.mjs https://aucl.greluc.me
//
// **It cannot tell you whether the relay is reachable from the internet.** It speaks to
// the signalling server over HTTPS and reports what that server advertises; whether a
// client outside the server's own network can actually reach the TURN port is a question
// only a client outside that network can answer. Run from the same line as the server,
// every packet takes the NAT hairpin and proves nothing about the path a player takes.

import { io } from 'socket.io-client';

const URL = process.argv[2] ?? 'https://aucl.greluc.me';
// Six characters is the shape of a real code, and digits are not in the alphabet Among Us
// draws from — so this cannot collide with a lobby somebody is actually playing in.
const LOBBY = 'PROBE0';

// Everything a person reads goes to stderr, so stdout carries one machine-readable line
// and nothing else. The live-check workflow feeds that line to a real TURN client.
const say = (line) => process.stderr.write(`${line}
`);
let failed = 0;
const check = (name, ok, detail = '') => {
	if (!ok) failed++;
	say(`${ok ? '  ok  ' : ' FAIL '} ${name}${detail ? ` — ${detail}` : ''}`);
};
const settle = (ms) => new Promise((r) => setTimeout(r, ms));

function connect() {
	return new Promise((resolve, reject) => {
		const socket = io(URL, { transports: ['websocket'], reconnection: false, timeout: 10000 });
		const peerConfig = new Promise((got) => socket.once('clientPeerConfig', got));
		socket.once('connect', () => resolve({ socket, peerConfig }));
		socket.once('connect_error', reject);
	});
}

const before = await fetch(`${URL}/health`).then((r) => r.json());
say(`server: uptime ${Math.round(before.uptime)}s, ${before.connectionCount} connected, ${before.lobbiesCount} lobbies`);
say(`probing as ${LOBBY}, two sockets, no lobby publication\n`);

const a = await connect();
const b = await connect();
check('two clients complete the websocket handshake', a.socket.connected && b.socket.connected);

const [ca, cb] = await Promise.all([a.peerConfig, b.peerConfig]);
check('both are sent a clientPeerConfig', Array.isArray(ca?.iceServers) && Array.isArray(cb?.iceServers));

// What a caller outside this process can act on -- see .github/workflows/live.yml, which
// hands it to coturn's own client from a runner outside the server's network. This probe
// reports what it was given; it cannot make an allocation itself.
let relayOffer = null;
const stun = ca.iceServers.filter((s) => String(s.urls).startsWith('stun:'));
const turn = ca.iceServers.filter((s) => String(s.urls).startsWith('turn:'));
say(`        advertised: ${ca.iceServers.map((s) => s.urls).join(', ')}`);
say(`        forceRelayOnly: ${ca.forceRelayOnly}`);
check('at least one STUN server is advertised', stun.length > 0);

if (turn.length > 0) {
	check('a relay is advertised over UDP and TCP', turn.length === 2, `${turn.length} entries`);
	const [ta, tb] = [ca, cb].map((c) => c.iceServers.find((s) => String(s.urls).startsWith('turn:')));
	// Two clients must not share a credential. When this first ran against the live
	// server they did: the username was the expiry alone, so everybody connecting in the
	// same second held the same one. Fixed by putting the socket id in the username;
	// this is what would catch it coming back.
	check(
		'each client gets its own relay credential',
		ta.username !== tb.username || ta.credential !== tb.credential,
		ta.username === tb.username ? `both were given "${ta.username}"` : ''
	);
	// The relay host has to be something a client elsewhere can resolve. A live run found
	// `turn:2cd620ec462e:3478` -- a container id, because the server read `HOSTNAME` and a
	// container runtime sets that itself. Nothing errored; the relay was simply
	// unreachable for everyone.
	const host = String(ta.urls).replace(/^turns?:/, '').split(':')[0];
	const looksLikeAContainerId = /^[0-9a-f]{12}$/.test(host);
	check(
		'the relay host is a name, not a container id',
		!looksLikeAContainerId,
		looksLikeAContainerId ? `advertised "${host}" — is PUBLIC_HOSTNAME set?` : host
	);

	relayOffer = { urls: turn.map((t) => t.urls), username: ta.username, credential: ta.credential };

	const expiry = Number(ta.username.split(':')[0]);
	if (Number.isFinite(expiry)) {
		const hours = ((expiry * 1000 - Date.now()) / 3_600_000).toFixed(1);
		say(`        credential expires in ${hours}h`);
		check('the credential has not already expired', expiry * 1000 > Date.now());
	}
} else {
	say('        no TURN relay advertised — direct connections only');
}

// --- the lobby path -------------------------------------------------------------------
const seenByA = new Set();
const signalsAtB = [];
a.socket.on('setClients', (m) => Object.keys(m).forEach((id) => seenByA.add(id)));
a.socket.on('join', (peer) => seenByA.add(peer));
b.socket.on('signal', (p) => signalsAtB.push(p));

a.socket.emit('join', LOBBY, 1, 901, true);
await settle(500);
b.socket.emit('join', LOBBY, 2, 902, false);
await settle(1200);

check('the first client is told when the second joins', seenByA.has(b.socket.id));

a.socket.emit('signal', { to: b.socket.id, data: { type: 'offer', sdp: 'v=0 live-probe' } });
await settle(1200);
check('a signal is relayed between them', signalsAtB.some((s) => s.data?.sdp === 'v=0 live-probe'));

a.socket.emit('leave');
b.socket.emit('leave');
await settle(400);
a.socket.disconnect();
b.socket.disconnect();
await settle(600);

const after = await fetch(`${URL}/health`).then((r) => r.json());
check(
	'the probe left nothing behind',
	after.lobbiesCount <= before.lobbiesCount,
	`${before.lobbiesCount} lobbies before, ${after.lobbiesCount} after`
);
check('and nothing was refused while it ran', after.counters.refusedSignals === before.counters.refusedSignals);

say(`\n${failed === 0 ? 'all checks passed' : `${failed} check(s) failed`}`);
// `process.exitCode` rather than `process.exit()`: exiting while socket.io is still
// tearing its handles down trips a libuv assertion on Windows, after every check has
// already run. Setting the code and letting the loop drain reports the same result
// without the noise.
// The one line on stdout, for a caller that can reach the relay from somewhere useful.
process.stdout.write(`${JSON.stringify({ relay: relayOffer })}
`);

process.exitCode = failed === 0 ? 0 : 1;
