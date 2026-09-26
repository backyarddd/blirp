// Terminal attach framing (ARCHITECTURE §6), isolated here so a framing change is one file.
// Server -> client: first a JSON text frame {"type":"snapshot","data":"..."}, then binary
// frames of raw PTY output. Client -> server: binary frames of input bytes and JSON text
// {"type":"resize","cols":N,"rows":N}.

export type ServerFrame =
  | { type: 'snapshot'; data: string }
  | { type: 'output'; data: Uint8Array }
  | { type: 'ignored'; reason: string };

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
  const { type, data } = parsed as { type?: unknown; data?: unknown };
  if (type === 'snapshot' && typeof data === 'string') return { type: 'snapshot', data };
  return { type: 'ignored', reason: `unknown frame type ${String(type)}` };
}

const encoder = new TextEncoder();

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
  if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 1 || rows < 1) {
    throw new RangeError(`invalid terminal size ${cols}x${rows}`);
  }
  return JSON.stringify({ type: 'resize', cols, rows });
}

/**
 * Close codes after which reconnecting is pointless. 1000: the terminal ended normally.
 * 1008/4401/4403: not authorized (or no terminal control). 4404: no such terminal.
 */
export type CloseKind = 'ended' | 'forbidden' | 'not_found' | 'retry';

export function classifyClose(code: number): CloseKind {
  if (code === 1000) return 'ended';
  if (code === 1008 || code === 4401 || code === 4403) return 'forbidden';
  if (code === 4404) return 'not_found';
  return 'retry';
}

/** Exponential backoff with equal jitter: a delay in [c/2, c] where c = min(max, base * 2^attempt). */
export function backoffDelay(attempt: number, base = 500, max = 15_000, rand: () => number = Math.random): number {
  const ceiling = Math.min(max, base * 2 ** Math.max(0, attempt));
  return Math.round(ceiling / 2 + (rand() * ceiling) / 2);
}
