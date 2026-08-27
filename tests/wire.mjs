// Drives the Rust server with the reference Socket.IO client, so the wire format is
// verified against the implementation the shipping clients actually use rather than
// against another copy of our own assumptions.
//
// Run by `tests/wire.rs`. Prints one JSON object of named checks and exits non-zero if
// any of them failed.

import { io } from 'socket.io-client';

const PORT = Number(process.argv[2]);
const URL = `http://127.0.0.1:${PORT}`;
const LOBBY = 'ABCDEF';

const checks = {};
const notes = {};
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function record(name, passed, note) {
	checks[name] = passed === true;
	if (note !== undefined) notes[name] = note;
}

/** Connects the way both shipping clients do. */
function connect(onPeerConfig) {
	return new Promise((resolve, reject) => {
		const socket = io(URL, { transports: ['websocket'], reconnection: false });
		if (onPeerConfig) socket.on('clientPeerConfig', onPeerConfig);
		socket.on('connect', () => resolve(socket));
		socket.on('connect_error', reject);
	});
}

/**
 * Resolves when the event arrives, or after the timeout.
 *
 * Checking for an event straight after `connect` resolves is a race: the two arrive in
 * whichever order the event loop delivers them, so a check written that way passes on
 * one machine and fails on the next.
 */
function waitFor(socket, event, ms = 2000) {
	return new Promise((resolve) => {
		const timer = setTimeout(() => resolve(null), ms);
		socket.once(event, (payload) => {
			clearTimeout(timer);
			resolve(payload);
		});
	});
}

const health = async () => (await fetch(`${URL}/health`)).json();

/**
 * Reads server-sent events until `count` of them carry data, or the time runs out.
 *
 * Comment frames — the keep-alive — carry no data field and are skipped here, which is
 * also what a browser's EventSource does with them.
 */
async function readSse(path, { lastEventId, count, ms = 4000 } = {}) {
	const controller = new AbortController();
	const timer = setTimeout(() => controller.abort(), ms);
	const events = [];
	try {
		const response = await fetch(`${URL}${path}`, {
			headers: lastEventId === undefined ? {} : { 'Last-Event-ID': String(lastEventId) },
			signal: controller.signal,
		});
		const reader = response.body.getReader();
		const decoder = new TextDecoder();
		let buffer = '';
		while (events.length < count) {
			const { value, done } = await reader.read();
			if (done) break;
			buffer += decoder.decode(value, { stream: true });
			let boundary = buffer.indexOf('\n\n');
			while (boundary !== -1) {
				const frame = buffer.slice(0, boundary);
				buffer = buffer.slice(boundary + 2);
				const event = {};
				for (const line of frame.split('\n')) {
					if (line.startsWith('id:')) event.id = line.slice(3).trim();
					else if (line.startsWith('data:')) event.data = (event.data ?? '') + line.slice(5).trim();
				}
				if (event.data !== undefined) events.push(event);
				boundary = buffer.indexOf('\n\n');
			}
		}
	} catch {
		// An abort is how this function ends when fewer events arrive than asked for.
	} finally {
		clearTimeout(timer);
		controller.abort();
	}
	return events;
}

async function main() {
	// --- the peer config arrives on connection --------------------------------------
	let peerConfig = null;
	let peerConfigResolved;
	const peerConfigSeen = new Promise((resolve) => {
		peerConfigResolved = resolve;
	});
	const a = await connect((config) => {
		peerConfig = config;
		peerConfigResolved();
	});
	await Promise.race([peerConfigSeen, wait(2000)]);
	record(
		'peer_config_on_connect',
		!!peerConfig && typeof peerConfig.forceRelayOnly === 'boolean' && Array.isArray(peerConfig.iceServers),
		peerConfig,
	);
	record(
		'peer_config_carries_the_default_stun_server',
		peerConfig?.iceServers?.some((server) => String(server.urls).startsWith('stun:')) === true,
		peerConfig?.iceServers,
	);

	const b = await connect();

	// --- join announces to the others, with two arguments ---------------------------
	const joins = [];
	const setClients = [];
	const hosts = [];
	a.on('join', (peerId, client) => joins.push({ peerId, client }));
	a.on('setClients', (clients) => setClients.push(clients));
	a.on('setHost', (hostId) => hosts.push(hostId));

	a.emit('join', LOBBY, 1, 101, true);
	await wait(250);
	const bSetClients = [];
	b.on('setClients', (clients) => bSetClients.push(clients));
	b.emit('join', LOBBY, 2, 102, false);
	await wait(400);

	record(
		'join_announced_with_two_arguments',
		joins.length === 1 && joins[0].peerId === b.id && joins[0].client?.clientId === 102,
		joins,
	);
	record(
		'set_clients_lists_the_existing_member',
		bSetClients.length === 1 && bSetClients[0]?.[a.id]?.clientId === 101,
		bSetClients,
	);
	record('host_claimed_by_first_claimer', hosts.includes(101), hosts);

	// --- the impostor radio, which is a 2.x event -------------------------------------
	//
	// Verified with the reference client for the reason this whole file exists: the shape
	// is what the shipping clients will parse, and checking it against another copy of our
	// own assumptions proves nothing. The Rust client's parser was written that way once
	// and read `VAD` as two positional arguments the server has never sent.
	//
	// 1.x carries this claim over the WebRTC data channel and never touches the socket for
	// it, so this event takes nothing away from it: a client that does not know the name
	// neither sends it nor hears it.
	const radioOn = waitFor(b, 'impostorRadio');
	a.emit('impostorRadio', true);
	const claimed = await radioOn;
	record(
		'impostor_radio_relayed_to_the_lobby',
		claimed?.onRadio === true &&
			claimed?.socketId === a.id &&
			claimed?.client?.clientId === 101,
		claimed,
	);

	// Releasing it has to arrive too. A radio that only ever switches on leaves an impostor
	// broadcasting to the other impostors after they believe they have stopped.
	const radioOff = waitFor(b, 'impostorRadio');
	a.emit('impostorRadio', false);
	const released = await radioOff;
	record('impostor_radio_release_relayed', released?.onRadio === false, released);

	// And it goes to the lobby rather than back to the claimant, like every other relayed
	// event here: a client that heard its own claim would count itself as being on the air.
	const echoed = waitFor(a, 'impostorRadio', 400);
	a.emit('impostorRadio', true);
	record('impostor_radio_not_echoed_to_the_sender', (await echoed) === null);
	a.emit('impostorRadio', false);
	await wait(200);

	// --- the signal envelope --------------------------------------------------------
	const relayedSoon = waitFor(b, 'signal');
	a.emit('signal', { to: b.id, data: { type: 'offer', sdp: 'v=0 probe' } });
	const relayed = await relayedSoon;
	record(
		'signal_relayed_to_a_co_member',
		relayed?.from === a.id && relayed?.data?.type === 'offer',
		relayed,
	);

	const before = (await health()).counters.refusedSignals;

	// A room name rather than a socket id: this is how the overlay feed and the mobile
	// relay addressed other clients, and it is what the envelope refuses.
	a.emit('signal', { to: `${LOBBY}_mobile`, data: { mobilePlayerInfo: { code: LOBBY } } });
	// Addressed to itself.
	a.emit('signal', { to: a.id, data: { type: 'offer' } });
	// A socket that exists but shares no lobby.
	const outsider = await connect();
	a.emit('signal', { to: outsider.id, data: { type: 'offer' } });
	await wait(400);

	const after = (await health()).counters.refusedSignals;
	record('envelope_refuses_three_kinds_of_target', after - before === 3, { before, after });

	let outsiderHeard = null;
	outsider.on('signal', (payload) => {
		outsiderHeard = payload;
	});
	await wait(200);
	record('outsider_received_nothing', outsiderHeard === null);
	outsider.disconnect();

	// --- departures -----------------------------------------------------------------
	const lefts = [];
	a.on('left', (peerId) => lefts.push(peerId));
	const bId = b.id;
	b.disconnect();
	await wait(500);
	record('hard_drop_announced', lefts.filter((id) => id === bId).length === 1, lefts);

	const c = await connect();
	const cId = c.id;
	c.emit('join', LOBBY, 3, 103, false);
	await wait(300);
	c.emit('leave');
	await wait(250);
	c.disconnect();
	await wait(400);
	record('explicit_leave_announced_once', lefts.filter((id) => id === cId).length === 1, lefts);

	// --- the public lobby list ------------------------------------------------------
	const browser = await connect();
	const updates = [];
	const removals = [];
	browser.on('update_lobby', (lobby) => updates.push(lobby));
	browser.on('remove_lobby', (id) => removals.push(id));
	browser.on('new_lobbies', (lobbies) => updates.push(...lobbies));
	browser.emit('lobbybrowser', true);
	await wait(250);

	a.emit('lobby', LOBBY, {
		title: 'A title that is quite a lot longer than twenty characters',
		host: 'greluc',
		current_players: 3.7,
		max_players: '15',
		language: 'german',
		mods: 'none',
		isPublic: true,
		server: URL,
		gameState: 0,
	});
	await wait(400);

	const listed = await (await fetch(`${URL}/lobbies`)).json();
	const published = listed[0];
	record(
		'lobby_published_and_coerced',
		listed.length === 1 &&
			published.title.length === 20 &&
			published.current_players === 3 &&
			published.max_players === 0 &&
			published.language === 'germa' &&
			published.mods === 'NONE' &&
			published.isPublic === true,
		published,
	);
	record('browser_saw_the_update', updates.some((lobby) => lobby.id === published.id), updates.length);

	// --- the stream ------------------------------------------------------------------
	// A subscriber with no position gets the whole list once and then follows along.
	const firstFrames = readSse('/lobbies/stream', { count: 2 });
	await wait(300);
	a.emit('lobby', LOBBY, {
		title: 'Second update',
		host: 'greluc',
		current_players: 4,
		max_players: 10,
		language: 'en',
		mods: 'NONE',
		isPublic: true,
		server: URL,
		gameState: 0,
	});
	const frames = await firstFrames;
	const snapshot = frames[0] ? JSON.parse(frames[0].data) : null;
	const follow = frames[1] ? JSON.parse(frames[1].data) : null;
	record(
		'stream_opens_with_a_snapshot',
		snapshot?.type === 'snapshot' && Array.isArray(snapshot.lobby) && snapshot.lobby.length === 1,
		snapshot,
	);
	record(
		'stream_then_carries_updates',
		follow?.type === 'update_lobby' && follow.lobby?.title === 'Second update',
		follow,
	);

	// A subscriber that returns with a position resumes from it instead of restarting.
	const resumeFrom = frames[1]?.id;
	const resumed = readSse('/lobbies/stream', { lastEventId: resumeFrom, count: 1 });
	await wait(300);
	a.emit('lobby', LOBBY, {
		title: 'Third update',
		host: 'greluc',
		current_players: 5,
		max_players: 10,
		language: 'en',
		mods: 'NONE',
		isPublic: true,
		server: URL,
		gameState: 0,
	});
	const resumedFrames = await resumed;
	const afterResume = resumedFrames[0] ? JSON.parse(resumedFrames[0].data) : null;
	record(
		'stream_resumes_from_last_event_id',
		afterResume?.type === 'update_lobby' && afterResume.lobby?.title === 'Third update',
		afterResume,
	);

	// --- the status page ---------------------------------------------------------------
	const statusResponse = await fetch(`${URL}/`);
	const statusPage = await statusResponse.text();
	record(
		'status_page_renders',
		statusResponse.status === 200 &&
			statusPage.includes('wire-test') &&
			statusPage.includes('<!doctype html>'),
		statusResponse.status,
	);

	// --- the lookup, and its cache header -------------------------------------------
	const codeResponse = await fetch(`${URL}/lobbies/${published.id}/code`);
	const codeBody = await codeResponse.json();
	record(
		'code_lookup_is_uncacheable',
		codeResponse.status === 200 &&
			codeBody.code === LOBBY &&
			codeResponse.headers.get('cache-control') === 'no-store',
		{ status: codeResponse.status, cache: codeResponse.headers.get('cache-control') },
	);

	// --- the acknowledgement fires exactly once -------------------------------------
	let ackCount = 0;
	let ackArgs = null;
	a.emit('join_lobby', published.id, (...args) => {
		ackCount += 1;
		ackArgs = args;
	});
	await wait(300);
	record(
		'join_lobby_acknowledged_once_with_four_arguments',
		ackCount === 1 && ackArgs?.[0] === 0 && ackArgs?.[1] === LOBBY && ackArgs?.length === 4,
		{ ackCount, ackArgs },
	);

	let missingAcks = 0;
	a.emit('join_lobby', 999999, () => {
		missingAcks += 1;
	});
	await wait(300);
	record('unknown_lobby_acknowledged_once', missingAcks === 1);

	// --- polling is refused ----------------------------------------------------------
	const pollingRefused = await new Promise((resolve) => {
		const socket = io(URL, { transports: ['polling'], reconnection: false });
		const timer = setTimeout(() => {
			socket.disconnect();
			resolve(false);
		}, 3000);
		socket.on('connect', () => {
			clearTimeout(timer);
			socket.disconnect();
			resolve(false);
		});
		socket.on('connect_error', () => {
			clearTimeout(timer);
			socket.disconnect();
			resolve(true);
		});
	});
	record('polling_handshake_refused', pollingRefused);

	// --- teardown leaves nothing behind ----------------------------------------------
	a.emit('remove_lobby', LOBBY);
	await wait(300);
	record('removal_announced', removals.includes(published.id), removals);

	browser.disconnect();
	a.disconnect();
	await wait(600);
	const final = await health();
	record(
		'counts_settle_to_zero',
		final.connectionCount === 0 && final.lobbiesCount === 0,
		{ connections: final.connectionCount, lobbies: final.lobbiesCount },
	);
}

try {
	await main();
} catch (error) {
	record('harness_completed', false, String(error?.stack ?? error));
}

const failed = Object.entries(checks).filter(([, passed]) => !passed);
console.log(JSON.stringify({ checks, notes, failed: failed.map(([name]) => name) }, null, 1));
process.exit(failed.length === 0 ? 0 : 1);
