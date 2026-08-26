// What the deployed pair actually does, driven by real socket.io clients.
//
// `wire.mjs` checks the protocol against a server started by cargo. This checks the
// *deployment*: the two container images, configured the way the quadlets configure them,
// with several clients in one lobby at once — and it ends by handing the TURN credential
// the server issued to coturn's own test client, which is the only way to find out
// whether the two halves of the shared-secret scheme actually agree.
//
// Driven by tests/deployment.sh, which brings the containers up and runs the allocation.
// Reads ACL_URL from the environment.

import { connectAll, harness, relayEntries, settle } from './lib/acl-client.mjs';

const { check, done } = harness();

const URL = process.env.ACL_URL ?? 'http://127.0.0.1:9736';
const LOBBY = 'DEPLOY';
const CLIENTS = 5;

const clients = await connectAll(URL, CLIENTS);
check(`${CLIENTS} clients connect over websocket`, clients.every((c) => c.socket.connected));

// Every one of them is told how to reach the relay. This is the part a single-client test
// cannot distinguish from a value cached at start-up.
const configs = await Promise.all(clients.map((c) => c.peerConfig));
check(
	'every client receives a clientPeerConfig',
	configs.every((c) => Array.isArray(c?.iceServers) && c.iceServers.length > 0)
);

const turnEntries = configs.map(relayEntries);
check(
	'each client is given a TURN relay over both transports',
	turnEntries.every((entries) => entries.length === 2),
	`saw ${turnEntries.map((e) => e.length).join(',')}`
);
check(
	'the two transports for one client share one credential',
	turnEntries.every(([udp, tcp]) => udp && tcp && udp.username === tcp.username && udp.credential === tcp.credential)
);

if (turnEntries.some((e) => e.length !== 2)) {
	// Without a relay in the advertisement there is nothing to hand coturn, so stop here
	// and say what was advertised instead. Carrying on would fail with a TypeError and
	// bury the actual finding.
	process.stderr.write("advertised instead: " + JSON.stringify(configs[0]?.iceServers) + "\n");
	process.exit(1);
}

// --- everyone in one lobby sees everyone else -----------------------------------------
//
// The bookkeeping this exercises is `setClients` for the joiner and `join` for everybody
// already there. With two clients a server that only ever answers the joiner looks
// correct; with five it does not.
const seen = clients.map(() => new Set());
const vadHeard = clients.map(() => []);
const signalsHeard = clients.map(() => []);

clients.forEach((c, i) => {
	c.socket.on('setClients', (map) => Object.keys(map).forEach((id) => seen[i].add(id)));
	c.socket.on('join', (peer) => seen[i].add(peer));
	c.socket.on('left', (peer) => seen[i].delete(peer));
	c.socket.on('VAD', (payload) => vadHeard[i].push(payload));
	c.socket.on('signal', (payload) => signalsHeard[i].push(payload));
});

for (const [i, c] of clients.entries()) {
	c.socket.emit('join', LOBBY, i + 1, 100 + i, i === 0);
	// Sequentially, because the assertion below is about what each *arrival* is told, and
	// a simultaneous burst would make the expected sets a race.
	await settle(150);
}
await settle(600);

const ids = clients.map((c) => c.socket.id);
const everyoneSeesEveryoneElse = clients.every((_, i) => {
	const expected = ids.filter((_, j) => j !== i).sort();
	return JSON.stringify([...seen[i]].sort()) === JSON.stringify(expected);
});
check(
	`each of the ${CLIENTS} clients ends up knowing the other ${CLIENTS - 1}`,
	everyoneSeesEveryoneElse,
	everyoneSeesEveryoneElse ? '' : seen.map((s) => s.size).join(',')
);

// --- a signal goes to one peer, and to exactly one ------------------------------------
clients[0].socket.emit('signal', { to: ids[3], data: { type: 'offer', sdp: 'v=0 deployment' } });
await settle(500);
check(
	'a signal reaches the addressed peer',
	signalsHeard[3].some((s) => s.data?.sdp === 'v=0 deployment' && s.from === ids[0])
);
check(
	'and reaches nobody else',
	signalsHeard.filter((_, i) => i !== 3).every((heard) => heard.length === 0),
	signalsHeard.map((h) => h.length).join(',')
);

// --- VAD fans out to the rest of the lobby --------------------------------------------
clients[1].socket.emit('VAD', true);
await settle(500);
check(
	'VAD reaches every other client in the lobby',
	vadHeard.filter((_, i) => i !== 1).every((heard) => heard.some((v) => v.socketId === ids[1] && v.activity === true))
);
check('and not back to the sender', vadHeard[1].length === 0);

// --- a departure is announced to the rest ---------------------------------------------
const leaving = ids[4];
clients[4].socket.disconnect();
await settle(800);
check(
	'the others are told when one leaves',
	clients.slice(0, 4).every((_, i) => !seen[i].has(leaving)),
	seen.slice(0, 4).map((s) => (s.has(leaving) ? 'still-there' : 'gone')).join(',')
);

const health = await fetch(`${URL}/health`).then((r) => r.json());
check('the server counted four connections after one left', health.connectionCount === CLIENTS - 1, String(health.connectionCount));
check(
	'nothing was refused for rate limiting during any of this',
	health.counters.refusedRateLimited === 0,
	String(health.counters.refusedRateLimited)
);

// The credential the *server* minted, for coturn to be asked about by the shell driver.
const [udp] = turnEntries[0];
process.stdout.write(
	JSON.stringify({ username: udp.username, credential: udp.credential, urls: udp.urls }) + '\n'
);

for (const c of clients) c.socket.disconnect();

done();
