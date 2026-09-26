const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

const pad = (n: number): string => String(n).padStart(2, '0');

/** Elapsed duration: "42s", "7m 05s", "2h 03m", "3d 4h". Negative input is treated as 0. */
export function formatElapsed(ms: number): string {
  const t = Math.max(0, Math.floor(ms / 1000));
  const d = Math.floor(t / 86_400);
  const h = Math.floor((t % 86_400) / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = t % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${pad(m)}m`;
  if (m > 0) return `${m}m ${pad(s)}s`;
  return `${s}s`;
}

function startOfDay(ts: number): number {
  const d = new Date(ts);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Relative time for lists: "just now", "5m ago", "3h ago", "Yesterday", "Mon", "Mar 4", "Mar 4, 2024". */
export function formatRelative(ts: number, now: number = Date.now()): string {
  const diff = now - ts;
  if (diff < MIN) return 'just now';
  if (diff < HOUR) return `${Math.floor(diff / MIN)}m ago`;
  const days = Math.round((startOfDay(now) - startOfDay(ts)) / DAY);
  if (days === 0) return `${Math.floor(diff / HOUR)}h ago`;
  if (days === 1) return 'Yesterday';
  const date = new Date(ts);
  if (days < 7) return date.toLocaleDateString(undefined, { weekday: 'short' });
  const sameYear = date.getFullYear() === new Date(now).getFullYear();
  return date.toLocaleDateString(undefined, sameYear ? { month: 'short', day: 'numeric' } : { month: 'short', day: 'numeric', year: 'numeric' });
}

export function formatDateTime(ts: number): string {
  return new Date(ts).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

export function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}
