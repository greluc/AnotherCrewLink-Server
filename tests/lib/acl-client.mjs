// What every socket.io-driven test here needs, in one place.
//
// `deployment.mjs` and `live-probe.mjs` had their own copy of all of this — the same
// connect helper, the same check harness, the same settle. Two copies of the code that
// decides whether a test passed is two things to keep in step, and they had already
// drifted: one wrote its results to stdout and the other to stderr, for no reason beyond
// having been written on different days.
//
// `wire.mjs` deliberately does not use this. It is driven by `wire.rs`, which reads a
// JSON report on stdout, so its output shape is a contract with a caller rather than a
// convenience for a reader.

import { io } from 'socket.io-client';

/** Waits, because "the server has had time to answer" is not otherwise observable. */
export const settle = (ms = 400) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * One client, connected, with its `clientPeerConfig` waiting.
 *
 * The peer config is captured as a promise before the connect resolves: it arrives
 * immediately after the socket.io CONNECT, and a listener attached afterwards can miss
 * it. That is not hypothetical — the server sends it from its connect handler.
 */
export function connect(url, { timeout = 10000 } = {}) {
	return new Promise((resolve, reject) => {
		const socket = io(url, { transports: ['websocket'], reconnection: false, timeout });
		const peerConfig = new Promise((got) => socket.once('clientPeerConfig', got));
		socket.once('connect', () => resolve({ socket, peerConfig }));
		socket.once('connect_error', reject);
	});
}

/** `count` clients, connected. */
export async function connectAll(url, count, options) {
	const clients = [];
	for (let i = 0; i < count; i++) clients.push(await connect(url, options));
	return clients;
}

/**
 * A check harness whose human output goes to stderr.
 *
 * Always stderr, so a caller can put something machine-readable on stdout without either
 * of them having to know about the other. Both of these scripts now do.
 */
export function harness() {
	let failed = 0;
	const say = (line) => process.stderr.write(`${line}\n`);
	const check = (name, ok, detail = '') => {
		if (!ok) failed++;
		say(`${ok ? '  ok  ' : ' FAIL '} ${name}${detail ? ` — ${detail}` : ''}`);
		return ok;
	};
	const done = () => {
		say(`\n${failed === 0 ? 'all checks passed' : `${failed} check(s) failed`}`);
		// `exitCode` rather than `exit()`: exiting while socket.io is still tearing its
		// handles down trips a libuv assertion on Windows, after every check has run.
		process.exitCode = failed === 0 ? 0 : 1;
		return failed;
	};
	return { say, check, done, failures: () => failed };
}

/**
 * The TURN entries of a peer config, and the one thing worth asserting about the pair.
 *
 * A relay is advertised twice, over UDP and TCP, and both entries must carry the same
 * credential: two different ones would mean a client that authenticates over UDP and not
 * over TCP, which fails only on the networks that fall back to TCP — the ones that needed
 * the relay in the first place.
 */
export function relayEntries(peerConfig) {
	return (peerConfig?.iceServers ?? []).filter((server) => server.username);
}

/** The host a `turn:` URL points at, for checking it is a name a client can resolve. */
export function relayHost(urls) {
	return String([].concat(urls)[0])
		.replace(/^turns?:/, '')
		.split('?')[0]
		.split(':')[0];
}
