import { describe, expect, it } from 'vitest';
import type { Session } from './api/types.gen';
import { handoffPendingLabel } from './handoff';

const base: Session = {
  id: 's',
  project_id: 'p',
  project_updated_at: 0,
  machine_id: 'here',
  agent: 'claude',
  agent_session_id: null,
  origin: 'blirp',
  cwd: '/w',
  title: null,
  title_updated_at: 0,
  status: 'idle',
  branch: null,
  worktree: null,
  transcript_path: null,
  started_at: 0,
  ended_at: null,
  last_activity_at: 1_000,
  exit_code: null,
  summary: null,
  distilled_through_seq: 0,
  tokens_in: 0,
  tokens_out: 0,
  cost_usd: 0,
  parent_session_id: null,
  stopped_by_user: false,
};

describe('handoff pending label', () => {
  it('says the daemon summarizes a local session whose summary is behind', () => {
    expect(handoffPendingLabel(base, 'here')).toBe('Summarizing session…');
    expect(handoffPendingLabel({ ...base, summary: { distilled_at: 500 } }, 'here')).toBe('Summarizing session…');
    expect(handoffPendingLabel({ ...base, summary: { distilled_at: 2_000 } }, 'here')).toBe('Starting…');
  });
  it("hands off another machine's session with its synced summary", () => {
    expect(handoffPendingLabel(base, 'elsewhere')).toBe('Starting…');
    expect(handoffPendingLabel(base, undefined)).toBe('Starting…');
  });
});
