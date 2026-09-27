import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Session } from './api/types.gen';
import { compactionDismissedAt, compactionHintVisible, dismissCompactionHint, handoffPendingLabel } from './handoff';

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
  compacted_at: null,
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

describe('fresh session suggestion after a compaction', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('shows for a running session that compacted, until that compaction is dismissed', () => {
    expect(compactionHintVisible(base, null)).toBe(false);
    const compacted = { ...base, compacted_at: 5_000 };
    expect(compactionHintVisible(compacted, null)).toBe(true);
    expect(compactionHintVisible({ ...compacted, status: 'completed' }, null)).toBe(false);
    expect(compactionHintVisible(compacted, 5_000)).toBe(false);
    // A new compaction brings it back.
    expect(compactionHintVisible({ ...compacted, compacted_at: 9_000 }, 5_000)).toBe(true);
  });

  it('remembers dismissals per session in this browser, and survives broken storage', () => {
    const stored = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => stored.get(k) ?? null,
      setItem: (k: string, v: string) => void stored.set(k, v),
    });
    expect(compactionDismissedAt('a')).toBeNull();
    dismissCompactionHint('a', 5_000);
    dismissCompactionHint('b', 7_000);
    dismissCompactionHint('a', 9_000);
    expect(compactionDismissedAt('a')).toBe(9_000);
    expect(compactionDismissedAt('b')).toBe(7_000);
    for (let i = 0; i < 250; i++) dismissCompactionHint(`s${i}`, i);
    expect(compactionDismissedAt('a')).toBeNull();
    expect(compactionDismissedAt('s249')).toBe(249);

    stored.set('blirp.compactionHint.dismissed', 'not json');
    expect(compactionDismissedAt('a')).toBeNull();
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    });
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    dismissCompactionHint('a', 1);
    expect(compactionDismissedAt('a')).toBeNull();
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });
});
