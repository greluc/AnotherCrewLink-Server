import type { GameState } from './gameState';

export interface PublicLobby {
	id: number;
	title: string;
	host: string;
	current_players: number;
	max_players: number;
	language: string;
	mods: string;
	isPublic: boolean;
	isPublic2?: boolean;
	server: string;
	gameState: GameState;
	stateTime: number;
}
