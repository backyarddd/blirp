import { describe, expect, it } from 'vitest';
import type { ProjectSummary, Session, SessionStatus } from './api/types.gen';
import {
  agentLabel,
  basename,
  canResume,
  groupSessions,
  hasTerminal,
  isLive,
  remoteLiveState,
  type LiveContext,
  isSubagent,
  notifiableTransition,
  previewSessions,
  REMOTE_LIVE_MS,
  sessionOrder,
  sessionStatusInfo,
  sessionTitle,
  statusInfo,
} from './status';

const ALL: SessionStatus[] = ['starting', 'working', 'idle', 'waiting', 'completed', 'failed', 'detached'];

describe('status mapping', () => {
  it('labels every status and pulses only while working', () => {
    for (const st of ALL) expect(statusInfo(st).label.length).toBeGreaterThan(0);
    expect(ALL.filter((st) => statusInfo(st).pulse)).toEqual(['starting', 'working']);
    expect(statusInfo('waiting').tone).toBe('waiting');
    expect(statusInfo('starting').tone).toBe('working');
  });
  it('knows which sessions are live and have a terminal', () => {
    expect(ALL.filter(isLive)).toEqual(['starting', 'working', 'idle', 'waiting']);
    expect(hasTerminal({ origin: 'blirp', status: 'idle' })).toBe(true);
    expect(hasTerminal({ origin: 'external', status: 'working' })).toBe(false);
    expect(hasTerminal({ origin: 'blirp', status: 'detached' })).toBe(false);
  });
  it('resumes ended sessions with an agent id, or blirp sessions by relaunching', () => {
    expect(canResume({ status: 'detached', agent_session_id: 'u', origin: 'external' })).toBe(true);
    expect(canResume({ status: 'completed', agent_session_id: null, origin: 'blirp' })).toBe(true);
    expect(canResume({ status: 'failed', agent_session_id: null, origin: 'external' })).toBe(false);
    expect(canResume({ status: 'working', agent_session_id: 'u', origin: 'blirp' })).toBe(false);
  });
  it('notifies on waiting and finish transitions only', () => {
    expect(notifiableTransition('working', 'waiting')).toBe(true);
    expect(notifiableTransition('working', 'completed')).toBe(true);
    expect(notifiableTransition('waiting', 'waiting')).toBe(false);
    expect(notifiableTransition('idle', 'working')).toBe(false);
  });
});

describe('labels', () => {
  it('formats agents, folders and fallback titles', () => {
    expect(agentLabel('claude')).toBe('Claude Code');
    expect(agentLabel('custom:mytool')).toBe('mytool');
    expect(agentLabel('unknown')).toBe('unknown');
    expect(basename('C:\\Users\\me\\My Project\\')).toBe('My Project');
    expect(basename('/home/me/app')).toBe('app');
    expect(sessionTitle({ title: null, agent: 'codex', cwd: '/w/app' })).toBe('Codex in app');
    expect(sessionTitle({ title: '  Fix login ', agent: 'codex', cwd: '/w/app' })).toBe('Fix login');
  });
});

describe('groupSessions', () => {
  const mk = (id: string, project_id: string): Session => ({
    id,
    project_id,
    machine_id: 'm',
    agent: 'claude',
    agent_session_id: null,
    origin: 'blirp',
    cwd: '/w',
    title: null,
    status: 'idle',
    branch: null,
    worktree: null,
    transcript_path: null,
    started_at: 0,
    ended_at: null,
    last_activity_at: 0,
    exit_code: null,
    summary: null,
    distilled_through_seq: 0,
    tokens_in: 0,
    tokens_out: 0,
    cost_usd: 0,
    parent_session_id: null,
    stopped_by_user: false,
  });
  const alpha: ProjectSummary = {
    id: 'a',
    name: 'Alpha',
    created_at: 0,
    updated_at: 0,
    deleted: false,
    paths: [],
    is_git: false,
    is_home: false,
    session_count: 1,
    live_session_count: 0,
    last_activity_at: null,
  };
  it('shows Stopped only for ended sessions the user stopped', () => {
    expect(sessionStatusInfo({ status: 'completed', stopped_by_user: true }).label).toBe('Stopped');
    expect(sessionStatusInfo({ status: 'completed', stopped_by_user: false }).label).toBe('Completed');
    expect(sessionStatusInfo({ status: 'idle', stopped_by_user: true }).label).toBe('Idle');
  });
  it('treats only ingested children as subagents', () => {
    const sub = { ...mk('c1', 'a'), origin: 'external' as const, parent_session_id: 'p' };
    const fork = { ...mk('c2', 'a'), parent_session_id: 'p' };
    expect(isSubagent(sub)).toBe(true);
    expect(isSubagent(fork)).toBe(false);
  });
  it('groups by project in first-seen order', () => {
    const groups = groupSessions([mk('1', 'b'), mk('2', 'a'), mk('3', 'b')], new Map([['a', alpha]]));
    expect(groups.map((g) => [g.name, g.sessions.map((s) => s.id)])).toEqual([
      ['Unknown project', ['1', '3']],
      ['Alpha', ['2']],
    ]);
  });
  it('orders live sessions first, then by last activity, then id (as the daemon does)', () => {
    const at = (id: string, status: SessionStatus, last: number, started = 0): Session => ({
      ...mk(id, 'a'),
      status,
      last_activity_at: last,
      started_at: started,
    });
    const list = [
      at('old', 'completed', 5),
      at('tie-a', 'failed', 50),
      at('idle-live', 'idle', 1),
      at('long-running', 'completed', 100, -1000),
      at('tie-b', 'completed', 50),
      at('working', 'working', 2),
    ];
    expect([...list].sort(sessionOrder({ selfId: 'm', machines: new Map(), now: 100 })).map((s) => s.id)).toEqual([
      'working',
      'idle-live',
      'long-running',
      'tie-b',
      'tie-a',
      'old',
    ]);
  });
  it('previews a group without hiding live or selected sessions', () => {
    const list = ['1', '2', '3', '4', '5'].map((id) => ({ ...mk(id, 'a'), status: 'completed' as const }));
    const live = { ...mk('6', 'a'), status: 'waiting' as const };
    const all = [...list, live];
    const p = previewSessions(all, 2, '4', { selfId: 'm', machines: new Map(), now: 0 });
    expect(p.shown.map((s) => s.id)).toEqual(['1', '2', '4', '6']);
    expect(p.hidden).toBe(2);
    expect(previewSessions(all, 10, null, { selfId: 'm', machines: new Map(), now: 0 })).toEqual({ shown: all, hidden: 0 });
    // A live session of a machine that is offline is not kept on screen.
    const offline = { ...live, machine_id: 'other' };
    const ctx = { selfId: 'm', machines: new Map([['other', { online: false }]]), now: 0 };
    expect(previewSessions([...list, offline], 2, null, ctx).hidden).toBe(4);
  });
  it("trusts another machine's live status while it is online, not once it is offline", () => {
    const now = 10 * REMOTE_LIVE_MS;
    const s = (id: string, machine_id: string, last: number, status: SessionStatus = 'working'): Session => ({
      ...mk(id, 'a'),
      machine_id,
      status,
      last_activity_at: last,
    });
    const ctx = (machines: Record<string, boolean | null>, selfId: string | null = 'm'): LiveContext => ({
      selfId,
      machines: new Map(Object.entries(machines).map(([id, online]) => [id, { online }])),
      now,
    });
    const mineIdle = s('mine', 'm', 0, 'idle');
    const quiet = s('quiet', 'other', now - REMOTE_LIVE_MS - 1);
    const fresh = s('fresh', 'other', now - 1);
    const ended = s('ended', 'm', now - 10, 'completed');
    // Online: an idle terminal quiet for hours is still running.
    expect(remoteLiveState(quiet, ctx({ other: true }))).toBeNull();
    // Offline: even a fresh report is only the last one it sent.
    expect(remoteLiveState(fresh, ctx({ other: false }))).toBe('offline');
    // Unknown presence: the activity window decides.
    expect(remoteLiveState(quiet, ctx({ other: null }))).toBe('stale');
    expect(remoteLiveState(quiet, ctx({}))).toBe('stale');
    expect(remoteLiveState(fresh, ctx({}))).toBeNull();
    // This machine, ended sessions and an unknown self never count.
    expect(remoteLiveState(mineIdle, ctx({ m: false }))).toBeNull();
    expect(remoteLiveState({ ...quiet, status: 'completed' }, ctx({ other: false }))).toBeNull();
    expect(remoteLiveState(quiet, ctx({ other: false }, null))).toBeNull();
    const order = (c: LiveContext): string[] => [ended, quiet, mineIdle, fresh].sort(sessionOrder(c)).map((x) => x.id);
    expect(order(ctx({ other: true }))).toEqual(['fresh', 'quiet', 'mine', 'ended']);
    expect(order(ctx({ other: false }))).toEqual(['mine', 'fresh', 'ended', 'quiet']);
    expect(order(ctx({}))).toEqual(['fresh', 'mine', 'ended', 'quiet']);
  });
});
