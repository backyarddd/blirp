// Grid page layout: tiles keep a stable, user-arranged order (per browser, like panes.ts) and
// are tiled as close to square as the width allows.
import { parseMarks } from './marks';
import { writePref } from './prefs';

const KEY = 'blirp.gridOrder';

export function readGridOrder(): string[] {
  try {
    return parseMarks(localStorage.getItem(KEY));
  } catch {
    return [];
  }
}

export function writeGridOrder(ids: readonly string[]): void {
  writePref(KEY, JSON.stringify(ids));
}

/** Arranged tiles first, in their saved order; the rest after them, oldest first. Status never moves a tile. */
export function orderTiles<T extends { id: string; started_at: number }>(sessions: readonly T[], saved: readonly string[]): T[] {
  const rank = new Map(saved.map((id, i) => [id, i]));
  const at = (s: T): number => rank.get(s.id) ?? Infinity;
  return [...sessions].sort((a, b) => at(a) - at(b) || a.started_at - b.started_at);
}

/** `ids` with the item at `from` moved to index `to`. */
export function moveTile(ids: readonly string[], from: number, to: number): string[] {
  const out = [...ids];
  const [id] = out.splice(from, 1);
  if (id !== undefined) out.splice(Math.max(0, Math.min(to, out.length)), 0, id);
  return out;
}

export interface GridLayout {
  /** CSS grid columns. */
  columns: number;
  /** Columns each tile spans, outside the last row. */
  span: number;
  /** Columns each tile in a short last row spans, so that row is filled too. */
  lastSpan: number;
  /** Index of the first tile in a short last row; `n` when every row is full. */
  lastFrom: number;
}

/**
 * Near-square tiling: ceil(sqrt(n)) columns (4 tiles 2x2, 9 tiles 3x3), fewer when the width
 * fits fewer. A short last row is stretched evenly (5 tiles in 3 columns: 3 + 2 wide ones).
 */
export function gridLayout(n: number, maxColumns: number): GridLayout {
  const cols = Math.max(1, Math.min(Math.ceil(Math.sqrt(n)), maxColumns));
  const last = n % cols;
  if (last === 0) return { columns: cols, span: 1, lastSpan: 1, lastFrom: n };
  return { columns: cols * last, span: last, lastSpan: cols, lastFrom: n - last };
}
