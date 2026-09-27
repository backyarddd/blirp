// Terminal attach framing (ARCHITECTURE §6), isolated here so a framing change is one file.
// Server -> client: JSON text frames `TerminalServerMessage` (`snapshot` first and again
// whenever this client fell behind, `readonly` right after the first snapshot when this client
// may not control the terminal, `resize` when another client resized, `exit` right before the
// socket closes) and binary frames of raw PTY output. Client -> server: binary frames of raw
// input bytes and JSON text `{"type":"resize","cols":N,"rows":N}` (1-1000 each).
//
// Close codes: 1000 after `exit`, 1001 on daemon shutdown or when this client's access changed
// (reconnect), 1011 when a relayed terminal's machine is unreachable; a refused upgrade
// (404 `terminal_not_found`, 401) surfaces as 1006. The `exit` frame says the process ended (also
// sent, then 1000, when attaching to a session whose process already ended).
import type { SessionStatus, TerminalClientMessage, TerminalServerMessage } from '../api/types.gen';

export type ServerFrame =
  | TerminalServerMessage
  | { type: 'output'; data: Uint8Array }
  | { type: 'ignored'; reason: string };

const STATUSES: ReadonlySet<string> = new Set<SessionStatus>([
  'starting',
  'working',
  'idle',
  'waiting',
  'completed',
  'failed',
  'detached',
]);

const isSize = (n: unknown): n is number => typeof n === 'number' && Number.isInteger(n) && n >= 1 && n <= 1000;
const isStatus = (s: unknown): s is SessionStatus => typeof s === 'string' && STATUSES.has(s);
const isExitCode = (c: unknown): c is number | null => c === null || (typeof c === 'number' && Number.isInteger(c));

export function decodeServerFrame(raw: string | ArrayBuffer | Uint8Array): ServerFrame {
  if (raw instanceof ArrayBuffer) return { type: 'output', data: new Uint8Array(raw) };
  if (raw instanceof Uint8Array) return { type: 'output', data: raw };
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { type: 'ignored', reason: 'invalid JSON text frame' };
  }
  if (typeof parsed !== 'object' || parsed === null) return { type: 'ignored', reason: 'non-object frame' };
  const f = parsed as { type?: unknown; cols?: unknown; rows?: unknown; data?: unknown; status?: unknown; exit_code?: unknown };
  switch (f.type) {
    case 'snapshot':
      if (isSize(f.cols) && isSize(f.rows) && typeof f.data === 'string')
        return { type: 'snapshot', cols: f.cols, rows: f.rows, data: f.data };
      break;
    case 'readonly':
      return { type: 'readonly' };
    case 'resize':
      if (isSize(f.cols) && isSize(f.rows)) return { type: 'resize', cols: f.cols, rows: f.rows };
      break;
    case 'exit':
      if (isStatus(f.status) && isExitCode(f.exit_code)) return { type: 'exit', status: f.status, exit_code: f.exit_code };
      break;
  }
  return { type: 'ignored', reason: `malformed or unknown frame type ${String(f.type)}` };
}

const encoder = new TextEncoder();

/** Keyboard/paste input, sent as a binary frame of UTF-8 bytes. */
export function encodeInput(data: string): Uint8Array<ArrayBuffer> {
  return encoder.encode(data);
}

/** xterm's onBinary delivers a latin1 string where each char is one byte. */
export function encodeBinaryInput(data: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i++) out[i] = data.charCodeAt(i) & 0xff;
  return out;
}

export function encodeResize(cols: number, rows: number): string {
  if (!isSize(cols) || !isSize(rows)) throw new RangeError(`invalid terminal size ${cols}x${rows}`);
  const msg: TerminalClientMessage = { type: 'resize', cols, rows };
  return JSON.stringify(msg);
}

/** Exponential backoff with equal jitter: a delay in [c/2, c] where c = min(max, base * 2^attempt). */
export function backoffDelay(attempt: number, base = 500, max = 15_000, rand: () => number = Math.random): number {
  const ceiling = Math.min(max, base * 2 ** Math.max(0, attempt));
  return Math.round(ceiling / 2 + (rand() * ceiling) / 2);
}
