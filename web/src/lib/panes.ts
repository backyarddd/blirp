// Split view: the sessions tiled side by side on the Sessions screen. The list is per browser
// (localStorage, like marks.ts); fewer than two panes means the single view.
import { parseMarks } from './marks';
import { writePref } from './prefs';

export const PANES_MAX = 4;
const KEY = 'blirp.panes';

/** The stored panes; unavailable storage or a single pane reads as no split. */
export function readPanes(): string[] {
  try {
    const ids = parseMarks(localStorage.getItem(KEY)).slice(0, PANES_MAX);
    return ids.length < 2 ? [] : ids;
  } catch {
    return [];
  }
}

export function writePanes(ids: readonly string[]): void {
  writePref(KEY, JSON.stringify(ids));
}

/** Open `id` beside `current`: the split starts with both, later ones are appended, a full split replaces its last pane. */
export function splitWith(panes: readonly string[], current: string | null, id: string): string[] {
  if (panes.includes(id)) return [...panes];
  if (panes.length === 0) return current !== null && current !== id ? [current, id] : [];
  return panes.length < PANES_MAX ? [...panes, id] : [...panes.slice(0, -1), id];
}

/** A session opened while split (sidebar click) takes the focused pane's place. */
export function followRoute(panes: readonly string[], focused: string | null, next: string): string[] {
  if (panes.length < 2 || panes.includes(next)) return [...panes];
  let i = focused !== null ? panes.indexOf(focused) : -1;
  if (i < 0) i = panes.length - 1;
  const out = [...panes];
  out[i] = next;
  return out;
}

/** Close a pane; one left ends the split. */
export function closePane(panes: readonly string[], id: string): string[] {
  const rest = panes.filter((x) => x !== id);
  return rest.length < 2 ? [] : rest;
}
