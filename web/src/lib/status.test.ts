import { describe, expect, it } from 'vitest';
import type { ProjectSummary, Session, SessionStatus } from './api/types.gen';
import {
  agentLabel,
  basename,
  canResume,
  childCount,
  groupSessions,
  hasTerminal,
  isLive,
  isSubagent,
  notifiableTransition,
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
    expect(sessionStatusInfo({ status: 'completed' }).label).toBe('Completed');
    expect(sessionStatusInfo({ status: 'idle', stopped_by_user: true }).label).toBe('Idle');
  });
  it('treats only ingested children as subagents and counts them', () => {
    const parent = mk('p', 'a');
    const sub = { ...mk('c1', 'a'), origin: 'external' as const, parent_session_id: 'p' };
    const fork = { ...mk('c2', 'a'), parent_session_id: 'p' };
    expect(isSubagent(sub)).toBe(true);
    expect(isSubagent(fork)).toBe(false);
    expect(childCount(parent, [parent, sub, fork])).toBe(1);
    expect(childCount({ ...parent, children_count: 4 }, [parent, sub])).toBe(4);
  });
  it('groups by project in first-seen order', () => {
    const groups = groupSessions([mk('1', 'b'), mk('2', 'a'), mk('3', 'b')], new Map([['a', alpha]]));
    expect(groups.map((g) => [g.name, g.sessions.map((s) => s.id)])).toEqual([
      ['Unknown project', ['1', '3']],
      ['Alpha', ['2']],
    ]);
  });
});
