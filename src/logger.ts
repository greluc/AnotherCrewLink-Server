import { format } from 'node:util';

/**
 * Replaces tracer, which was last published in 2023. The server only ever used
 * printf-style formatting at three levels, which util.format already provides.
 */

const COLOURS = {
	info: '\u001b[32m', // green
	warn: '\u001b[33m', // yellow
	error: '\u001b[31m', // red
} as const;
const RESET = '\u001b[0m';

// Disable colour when the output is redirected, so log files stay readable.
const useColour = process.stdout.isTTY === true && process.env.NO_COLOR === undefined;

function emit(level: keyof typeof COLOURS, args: unknown[]): void {
	const line = `${new Date().toISOString()} <${level}> ${format(...(args as [unknown]))}`;
	const stream = level === 'info' ? console.log : console.error;
	stream(useColour ? `${COLOURS[level]}${line}${RESET}` : line);
}

export const logger = {
	info: (...args: unknown[]) => emit('info', args),
	warn: (...args: unknown[]) => emit('warn', args),
	error: (...args: unknown[]) => emit('error', args),
};

export default logger;
