import { describe, expect, it } from 'vitest';
import type { Project, Session, SessionStatus } from './api/types';
import { agentLabel, basename, canResume, groupSessions, hasTerminal, isLive, notifiableTransition, sessionTitle, statusInfo } from './status';

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
  it('allows resume only with an agent session id', () => {
    expect(canResume({ status: 'detached', agent_session_id: 'u' })).toBe(true);
    expect(canResume({ status: 'detached', agent_session_id: null })).toBe(false);
    expect(canResume({ status: 'working', agent_session_id: 'u' })).toBe(false);
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
    started_at: 0,
    ended_at: null,
    last_activity_at: 0,
    exit_code: null,
    tokens_in: 0,
    tokens_out: 0,
    cost_usd: 0,
    parent_session_id: null,
  });
  const alpha: Project = {
    id: 'a',
    name: 'Alpha',
    created_at: 0,
    updated_at: 0,
    paths: [],
    is_git: false,
    session_count: 1,
    live_session_count: 0,
    last_activity_at: null,
    machine_ids: [],
  };
  it('groups by project in first-seen order', () => {
    const groups = groupSessions([mk('1', 'b'), mk('2', 'a'), mk('3', 'b')], new Map([['a', alpha]]));
    expect(groups.map((g) => [g.name, g.sessions.map((s) => s.id)])).toEqual([
      ['Unknown project', ['1', '3']],
      ['Alpha', ['2']],
    ]);
  });
});
