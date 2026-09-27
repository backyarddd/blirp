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

/** Why a session's context is worth a fresh start, and since when (the latest signal). */
export interface ContextSignal {
  kind: 'compacted' | 'near_full';
  at: number;
}

/**
 * The latest context signal of a session: its agent compacted its context (`compacted_at`), or its
 * context crossed 90% of the window the agent reports (`context_near_full_at`, codex).
 */
export function contextSignal(session: Pick<Session, 'compacted_at' | 'context_near_full_at'>): ContextSignal | null {
  const { compacted_at: compacted, context_near_full_at: full } = session;
  if (full !== null && (compacted === null || full > compacted)) return { kind: 'near_full', at: full };
  return compacted === null ? null : { kind: 'compacted', at: compacted };
}

/**
 * Whether a session shows the "start a fresh session" suggestion: it is running and has a context
 * signal newer than the one the user dismissed the suggestion for.
 */
export function compactionHintVisible(
  session: Pick<Session, 'status' | 'compacted_at' | 'context_near_full_at'>,
  dismissedAt: number | null,
): boolean {
  const at = contextSignal(session)?.at ?? null;
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
