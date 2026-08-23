import { spawn, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { type Socket, io } from 'socket.io-client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

// Drives the built server the way a client does. The lobby bookkeeping is only
// observable from the outside, so this starts the real process rather than importing
// it: src/index.ts opens its listener on import and has no exported handles.

const PORT = 19736;
const URL = `http://127.0.0.1:${PORT}`;
const LOBBY = 'ABCDEF';
const root = join(dirname(fileURLToPath(import.meta.url)), '..');

let server: ChildProcess;

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitForHealth(timeout = 15000) {
	const deadline = Date.now() + timeout;
	while (Date.now() < deadline) {
		try {
			const response = await fetch(`${URL}/health`);
			if (response.ok) return;
		} catch {
			/* not listening yet */
		}
		await wait(100);
	}
	throw new Error('server did not start');
}

const health = async () =>
	(await fetch(`${URL}/health`)).json() as Promise<{ connectionCount: number; lobbiesCount: number }>;

function connect(): Promise<Socket> {
	return new Promise((resolve, reject) => {
		const socket = io(URL, { transports: ['websocket'], reconnection: false });
		socket.on('connect', () => resolve(socket));
		socket.on('connect_error', reject);
	});
}

beforeAll(async () => {
	server = spawn(process.execPath, [join(root, 'dist', 'index.js')], {
		cwd: root,
		env: { ...process.env, PORT: String(PORT) },
		stdio: 'ignore',
	});
	await waitForHealth();
}, 30000);

afterAll(() => {
	server?.kill();
});

describe('lobby membership', () => {
	it('tells the others when someone joins', async () => {
		const [a, b] = [await connect(), await connect()];
		const joins: string[] = [];
		a.on('join', (peer: string) => joins.push(peer));

		a.emit('join', LOBBY, 1, 101, true);
		await wait(150);
		b.emit('join', LOBBY, 2, 102, false);
		await wait(300);

		expect(joins).toEqual([b.id]);
		a.disconnect();
		b.disconnect();
		await wait(200);
	});

	it('tells the others when someone drops without saying goodbye', async () => {
		// Before this was announced, the only sign of a departure was the peer connection
		// failing, which is indistinguishable from a connection that broke while both
		// players are still in the lobby.
		const [a, b] = [await connect(), await connect()];
		const left: string[] = [];
		a.on('left', (peer: string) => left.push(peer));

		a.emit('join', LOBBY, 1, 101, true);
		await wait(150);
		b.emit('join', LOBBY, 2, 102, false);
		await wait(200);

		const bId = b.id;
		b.disconnect();
		await wait(400);

		expect(left).toEqual([bId]);
		a.disconnect();
		await wait(200);
	});

	it('announces an explicit leave exactly once', async () => {
		const [a, b] = [await connect(), await connect()];
		const left: string[] = [];
		a.on('left', (peer: string) => left.push(peer));

		a.emit('join', LOBBY, 1, 101, true);
		await wait(150);
		b.emit('join', LOBBY, 2, 102, false);
		await wait(200);

		const bId = b.id;
		b.emit('leave');
		await wait(200);
		// The disconnect that follows must not announce the same departure a second time.
		b.disconnect();
		await wait(400);

		expect(left).toEqual([bId]);
		a.disconnect();
		await wait(200);
	});

	it('forgets a lobby once everyone is gone', async () => {
		const a = await connect();
		a.emit('join', LOBBY, 1, 101, true);
		await wait(200);
		expect((await health()).lobbiesCount).toBe(1);

		a.disconnect();
		await wait(300);
		const after = await health();
		expect(after.lobbiesCount).toBe(0);
		expect(after.connectionCount).toBe(0);
	});
});
