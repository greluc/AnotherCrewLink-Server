import dotenv from 'dotenv';
dotenv.config();
import express from 'express';
import { Server } from 'node:http';
import { Server as HttpsServer } from 'node:https';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { Server as SocketIOServer, type Socket } from 'socket.io';
import logger from './logger';
import morgan from 'morgan';
import peerConfig from './peerConfig';
import type { ICEServer } from './ICEServer';
import type { PublicLobby } from './interfaces/publicLobby';
import { GameState } from './interfaces/gameState';
import type { lobbyInfo } from './interfaces/lobbyInfo';

const httpsEnabled = !!process.env.HTTPS;

const port = process.env.PORT || (httpsEnabled ? '443' : '9736');

const sslCertificatePath = process.env.SSLPATH || process.cwd();

const app = express();
let server: HttpsServer | Server;
if (httpsEnabled) {
	server = new HttpsServer(
		{
			key: readFileSync(join(sslCertificatePath, 'privkey.pem')),
			cert: readFileSync(join(sslCertificatePath, 'fullchain.pem')),
		},
		app
	);
} else {
	server = new Server(app);
}

// The TURN relay runs as its own service (coturn); see docker-compose.yml. This only
// advertises it to clients.
const hostname = process.env.HOSTNAME;
const relayHost = peerConfig.relay.host || hostname;
const relayConfigured = peerConfig.relay.enabled && !!relayHost;
if (peerConfig.relay.enabled && !relayHost) {
	logger.error('relay.enabled is set but no relay.host and no HOSTNAME environment variable.');
}
if (relayConfigured && (!peerConfig.relay.username || !peerConfig.relay.credential)) {
	logger.error('relay.enabled is set but relay.username or relay.credential is empty.');
}

// socket.io 4 enforces CORS by default. The Electron renderer loads over file://,
// which sends a null origin, so it has to be allowed explicitly.
const io = new SocketIOServer(server, {
	cors: { origin: '*', methods: ['GET', 'POST'] },
});
const clients = new Map<string, Client>();
const publicLobbies = new Map<string, PublicLobby>();
const lobbyCodes = new Map<number, string>();
const allLobbies = new Map<string, lobbyInfo>();
let lobbyCount = 0;

function removePublicLobby(c: string) {
	const lobby = publicLobbies.get(c);
	if (lobby) {
		const pid = lobby.id;
		io.sockets.in('lobbybrowser').emit('remove_lobby', pid);
		lobbyCodes.delete(pid);
		publicLobbies.delete(c);
	}
}
interface Client {
	playerId: number;
	clientId: number;
}

interface Signal {
	data: string;
	to: string;
}

interface ClientPeerConfig {
	forceRelayOnly: boolean;
	iceServers: ICEServer[];
}

app.enable('trust proxy');
app.set('views', join(__dirname, '../views'));
app.use('/public', express.static(join(__dirname, '../public')));
app.set('view engine', 'pug');
app.use(morgan('combined'));

let connectionCount = 0;

app.get('/', (req, res) => {
	const address = `${req.protocol}://${req.hostname}`;
	res.render('index', { connectionCount, address, lobbiesCount: allLobbies.size });
});

app.get('/health', (req, res) => {
	const address = `${req.protocol}://${req.hostname}`;
	res.json({
		uptime: process.uptime(),
		connectionCount,
		lobbiesCount: allLobbies.size,
		address,
		name: process.env.NAME,
	});
});

app.get('/lobbies', (_req, res) => {
	res.json(Array.from(publicLobbies.values()));
});

/**
 * Coerces one field of an incoming lobby payload. Clients are unauthenticated, so
 * every field is whatever they chose to send; calling .substring() on a number threw
 * straight out of the socket handler and took the process down with it.
 */
function asText(value: unknown, maxLength: number): string {
	return typeof value === 'string' ? value.substring(0, maxLength) : '';
}

function asCount(value: unknown): number {
	return typeof value === 'number' && Number.isFinite(value) ? Math.max(0, Math.trunc(value)) : 0;
}

const leaveroom = (socket: Socket, code: string | null) => {
	if (!code) {
		return;
	}
	if (code && (code.length === 6 || code.length === 4)) socket.leave(code);

	if ((io.sockets.adapter.rooms.get(code)?.size ?? 0) <= 0) {
		if (allLobbies.has(code)) {
			allLobbies.delete(code);
		}
		removePublicLobby(code);
	}
};
// A throw inside a socket handler reaches the process as an uncaught exception and
// ends it. Clients are unauthenticated, so that turns any unhandled edge case into a
// remote denial of service. Log and keep serving instead.
process.on('uncaughtException', (error) => {
	logger.error('Uncaught exception, continuing: %s', error instanceof Error ? error.stack : String(error));
});
process.on('unhandledRejection', (reason) => {
	logger.error('Unhandled rejection, continuing: %s', reason instanceof Error ? reason.stack : String(reason));
});

io.on('connection', (socket: Socket) => {
	connectionCount++;
	logger.info('Total connected: %d in %d lobbies', connectionCount, allLobbies.size);
	let code: string | null = null;

	const clientPeerConfig: ClientPeerConfig = {
		forceRelayOnly: peerConfig.forceRelayOnly,
		iceServers: peerConfig.iceServers ? [...peerConfig.iceServers] : [],
	};

	if (relayConfigured) {
		clientPeerConfig.iceServers.push({
			urls: `turn:${relayHost}:${peerConfig.relay.port}`,
			username: peerConfig.relay.username,
			credential: peerConfig.relay.credential,
		});
	}

	socket.emit('clientPeerConfig', clientPeerConfig);

	socket.on('join', (c: string, id: number, clientId: number, isHost?: boolean) => {
		if (typeof c !== 'string' || typeof id !== 'number' || typeof clientId !== 'number') {
			socket.disconnect();
			logger.error(`Socket %s sent invalid join command: %s %d %d`, socket.id, c, id, clientId);
			return;
		}

		const otherClients: Record<string, Client | undefined> = {};
		const socketsInLobby = io.sockets.adapter.rooms.get(c);
		if (socketsInLobby) {
			for (const s of socketsInLobby) {
				if (s !== socket.id) otherClients[s] = clients.get(s);
			}
		}

		if (!allLobbies.has(c)) {
			allLobbies.set(c, { code: c, hostId: isHost ? clientId : -1, publicLobbyId: -1, connectedCount: 1 });
		} else {
			const lobby = allLobbies.get(c)!;
			lobby.connectedCount++;
			if (isHost) {
				lobby.hostId = clientId;
				// `c`, not `code`: code still holds the room this socket was in before
				// this join, so the host announcement went to the wrong room entirely.
				socket.to(c).emit('setHost', clientId);
			}
			socket.emit('setHost', lobby.hostId);
		}

		if (code != c) leaveroom(socket, code);
		code = c;
		socket.join(code);
		socket.to(code).emit('join', socket.id, {
			playerId: id,
			clientId: clientId,
		});
		socket.emit('setClients', otherClients);
	});

	socket.on('setHost', (c: string, clientId: number) => {
		if (code === c) {
			const lobby = allLobbies.get(c);
			if (lobby) {
				lobby.hostId = clientId;
				socket.to(c).emit('setHost', clientId);
			}
		}
	});

	socket.on('id', (id: number, clientId: number) => {
		if (typeof id !== 'number' || typeof clientId !== 'number') {
			socket.disconnect();
			logger.error(`Socket %s sent invalid id command: %d %d`, socket.id, id, clientId);
			return;
		}
		let client = clients.get(socket.id);
		if (client != null && client.clientId != null && client.clientId !== clientId) {
			///			socket.disconnect();
			logger.error(
				`Socket ${socket.id}->${client.clientId}->${clientId}->${id} sent invalid id command, attempted spoofing another client`
			);
			//			return;
		}
		client = {
			playerId: id,
			clientId: clientId,
		};
		clients.set(socket.id, client);
		if (code) {
			socket.to(code).emit('setClient', socket.id, client);
		}
	});

	socket.on('leave', () => {
		if (code) {
			leaveroom(socket, code);
			clients.delete(socket.id); // @ts-ignore
		}
	});

	socket.on('VAD', (activity: boolean) => {
		const client = clients.get(socket.id);
		if (code && client) {
			socket.to(code).emit('VAD', {
				activity,
				client,
				socketId: socket.id,
			});
		}
	});

	socket.on('join_lobby', (id: number, callbackFn) => {
		//ban check etc...
		const lobbyCode = lobbyCodes.get(id);
		const publicLobby = lobbyCode === undefined ? undefined : publicLobbies.get(lobbyCode);
		if (lobbyCode !== undefined && publicLobby) {
			if (publicLobby.isPublic && publicLobby.gameState === GameState.LOBBY) {
				callbackFn(0, lobbyCode, publicLobby.server, publicLobby);
				return;
			} else {
				callbackFn(1, 'Lobby is not public anymore');
			}
		}
		callbackFn(1, 'Lobby not found :C');
	});

	socket.on('lobby', (c: string, publicLobby: PublicLobby) => {
		if (typeof c !== 'string' || typeof publicLobby !== 'object' || publicLobby === null) {
			logger.error('Socket %s sent an invalid lobby command', socket.id);
			return;
		}
		if (code != c) {
			logger.error(`Got request to host lobby while not in it %s`, c, code);
			return;
		}
		if (!publicLobby.isPublic && !publicLobby.isPublic2) {
			removePublicLobby(c);
		} else {
			const publobby = publicLobbies.has(c) ? publicLobbies.get(c) : undefined;
			const id = publobby ? publobby.id : lobbyCount++;
			const stateTime =
				publobby &&
				((publobby.gameState === GameState.LOBBY && publicLobby.gameState === GameState.LOBBY) ||
					(publobby.gameState !== GameState.LOBBY && publicLobby.gameState !== GameState.LOBBY))
					? publobby.stateTime
					: Date.now();
			const lobby: PublicLobby = {
				id,
				title: asText(publicLobby.title, 20) || 'ERROR',
				host: asText(publicLobby.host, 10),
				current_players: asCount(publicLobby.current_players),
				max_players: asCount(publicLobby.max_players),
				language: asText(publicLobby.language, 5),
				mods: asText(publicLobby.mods, 20).toUpperCase(),
				isPublic: publicLobby.isPublic === true || publicLobby.isPublic2 === true,
				server: asText(publicLobby.server, 100),
				gameState: asCount(publicLobby.gameState),
				stateTime,
			};
			lobbyCodes.set(id, c);
			publicLobbies.set(c, lobby);
			io.sockets.in('lobbybrowser').emit('update_lobby', lobby);
		}
	});

	socket.on('remove_lobby', (c: string) => {
		if (code != c) {
			logger.error(`Got request to host lobby while not in it %s`, c, code);
			return;
		}
		removePublicLobby(c);
	});

	socket.on('signal', (signal: Signal) => {
		if (typeof signal !== 'object' || !signal.data || !signal.to || typeof signal.to !== 'string') {
			socket.disconnect();
			logger.error(`Socket %s sent invalid signal command: %j`, socket.id, signal);
			return;
		}
		const { to, data } = signal;
		io.to(to).emit('signal', {
			data,
			from: socket.id,
		});
	});

	socket.on('lobbybrowser', (open) => {
		if (!open) {
			socket.leave('lobbybrowser');
		} else {
			socket.join('lobbybrowser');
			io.sockets.in('lobbybrowser').emit('new_lobbies', Array.from(publicLobbies.values()));
		}
	});

	socket.on('disconnect', () => {
		leaveroom(socket, code);
		clients.delete(socket.id);
		connectionCount--;
		logger.info('Total connected: %d in %d lobbies', connectionCount, allLobbies.size);
	});
});

server.listen(port);
logger.info('AnotherCrewLink Server started: 127.0.0.1:%s', port);
