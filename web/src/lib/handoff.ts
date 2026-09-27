import type { Session } from './api/types.gen';
import { parseSummary } from './memory';

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
