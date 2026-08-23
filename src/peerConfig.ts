import YAML from 'yaml';
import path from 'node:path';
import fs from 'node:fs';
import type { ICEServer } from './ICEServer';

const PEER_CONFIG_PATH = path.join(__dirname, '..', 'config', 'peerConfig.yml');

// The TURN relay is a separate service now (coturn), not an embedded one. This only
// describes the relay that clients should be told about.
interface RelaySettings {
	enabled: boolean;
	/** Defaults to the HOSTNAME environment variable when omitted. */
	host?: string;
	port: number;
	username: string;
	credential: string;
}

interface PeerConfig {
	forceRelayOnly: boolean;
	relay: RelaySettings;
	iceServers?: ICEServer[];
}

const DEFAULT_PEER_CONFIG: PeerConfig = {
	forceRelayOnly: false,
	relay: {
		enabled: false,
		port: 3478,
		username: '',
		credential: '',
	},
	iceServers: [{ urls: 'stun:stun.l.google.com:19302' }],
};

let peerConfig = DEFAULT_PEER_CONFIG;
if (fs.existsSync(PEER_CONFIG_PATH)) {
	try {
		peerConfig = YAML.parse(fs.readFileSync(PEER_CONFIG_PATH).toString('utf8'));
	} catch (err) {
		console.error(`Unable to load peer config file. Make sure it is valid YAML.\n${err}`);
	}
}

export default peerConfig;
