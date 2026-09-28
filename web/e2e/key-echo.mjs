// A stand-in agent for keys.spec.ts: puts its terminal in raw mode, optionally switches terminal
// modes on the way full-screen TUIs do, and appends every input chunk it receives, hex encoded, as
// one line to the file named by its first argument ("ready" once it listens).
// Usage: key-echo.mjs <output file> [plain|app|kitty]
import { appendFileSync } from 'node:fs';

const [out, mode = 'plain'] = process.argv.slice(2);
if (!out) throw new Error('usage: key-echo.mjs <output file> [plain|app|kitty]');
const MODES = {
  plain: '',
  // Application cursor keys and keypad, bracketed paste, SGR mouse with button tracking, focus
  // reports, alternate screen.
  app: '\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h',
  // Kitty keyboard protocol, flags 1 (disambiguate escape codes), as Claude Code and Codex push.
  kitty: '\x1b[>1u',
};
const seq = MODES[mode];
if (seq === undefined) throw new Error(`unknown mode ${mode}`);
process.stdin.setRawMode(true);
process.stdin.on('data', (chunk) => appendFileSync(out, `${chunk.toString('hex')}\n`));
process.stdout.write(`${seq}key-echo ${mode} ready\r\n`);
appendFileSync(out, 'ready\n');
