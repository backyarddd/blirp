import type { Session } from './api/types.gen';
import { parseSummary } from './memory';
import { isLive } from './status';

/**
 * Label of a pending "Start new session from this session" launch. The daemon first distills a
 * session of its own machine whose summary is older than its latest activity (§9, bounded), so
 * such a launch can take a while; another machine's session is handed off with its synced summary.
 */
export function handoffPendingLabel(session: Session, localMachine: string | undefined): string {
  if (session.machine_id !== localMachine) return 'Starting…';
  const distilledAt = parseSummary(session.summary)?.distilled_at ?? null;
  return distilledAt === null || distilledAt < session.last_activity_at ? 'Summarizing session…' : 'Starting…';
}

/** Per browser: session id -> `compacted_at` of the compaction whose suggestion was dismissed. */
const DISMISSED_KEY = 'blirp.compactionHint.dismissed';
/** Dismissals kept (oldest dropped first); only running sessions ever show the suggestion. */
const DISMISSED_MAX = 200;

/**
 * Whether a session shows the "start a fresh session" suggestion: it is running and its agent
 * compacted its context (a compaction summary in the transcript, `compacted_at`) after the last
 * compaction the user dismissed the suggestion for.
 */
export function compactionHintVisible(
  session: Pick<Session, 'status' | 'compacted_at'>,
  dismissedAt: number | null,
): boolean {
  const at = session.compacted_at;
  return isLive(session.status) && at !== null && (dismissedAt === null || at > dismissedAt);
}

function readDismissals(): Record<string, number> {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(DISMISSED_KEY) ?? '{}');
    if (typeof v !== 'object' || v === null || Array.isArray(v)) return {};
    return Object.fromEntries(Object.entries(v).filter((e): e is [string, number] => typeof e[1] === 'number'));
  } catch {
    // Unreadable or unavailable storage: nothing was dismissed.
    return {};
  }
}

/** `compacted_at` of the compaction the suggestion was dismissed for, or null. */
export function compactionDismissedAt(sessionId: string): number | null {
  return readDismissals()[sessionId] ?? null;
}

/** Hide the suggestion for this session until its agent compacts again. */
export function dismissCompactionHint(sessionId: string, compactedAt: number): void {
  const all = readDismissals();
  delete all[sessionId];
  all[sessionId] = compactedAt;
  const kept = Object.entries(all).slice(-DISMISSED_MAX);
  try {
    localStorage.setItem(DISMISSED_KEY, JSON.stringify(Object.fromEntries(kept)));
  } catch (e) {
    console.warn('blirp: could not remember the dismissed suggestion', e);
  }
}
