import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Machine, ProjectSummary, Session } from './api/types.gen';
import {
  folderOn,
  forgetOpenSession,
  readOpenSessions,
  recentFolders,
  rememberOpenSession,
  remoteMachine,
  sessionToRestore,
  withOpened,
} from './machines';

const machine = (id: string, name: string, role: Machine['role']): Machine => ({
  id,
  name,
  os: 'macos',
  role,
  last_seen: 0,
  revoked: false,
});

const session = (id: string, machine_id: string, cwd: string, last_activity_at: number, worktree: string | null = null): Session =>
  ({ id, machine_id, cwd, last_activity_at, worktree }) as Session;

const project = (paths: { machine_id: string; path: string }[]): ProjectSummary => ({
  id: 'p',
  name: 'p',
  created_at: 0,
  updated_at: 0,
  deleted: false,
  paths: paths.map((p) => ({ ...p, git_remote: null, is_git: false, local: false })),
  is_git: false,
  is_home: false,
  workspace: null,
  chats: false,
  session_count: 0,
  live_session_count: 0,
  last_activity_at: null,
});

describe('remote machines', () => {
  const machines = new Map([
    ['hub', machine('hub', 'Server', 'hub')],
    ['lap', machine('lap', 'Laptop', 'node')],
  ]);
  it('badges only other machines, the hub as cloud', () => {
    expect(remoteMachine('pc', 'pc', machines, 'hub')).toBeNull();
    expect(remoteMachine('hub', 'pc', machines, 'hub')).toEqual({ id: 'hub', name: 'Server', cloud: true });
    expect(remoteMachine('lap', 'pc', machines, 'hub')).toEqual({ id: 'lap', name: 'Laptop', cloud: false });
    expect(remoteMachine('gone1234567', 'pc', machines, 'hub')?.name).toBe('machine gone1234');
    expect(remoteMachine('hub', null, machines, 'hub')).toBeNull();
  });
});

describe('recent folders', () => {
  it('lists a machine’s session folders newest first, then its project folders', () => {
    const sessions = [
      session('1', 'hub', '/Users/me/a', 10),
      session('2', 'hub', '/Users/me/b', 30),
      session('3', 'pc', 'C:\\x', 40),
      session('4', 'hub', '/Users/me/a', 20),
      session('5', 'hub', '/Users/me/.blirp/worktrees/p/w', 50, '/Users/me/.blirp/worktrees/p/w'),
    ];
    const projects = [project([{ machine_id: 'hub', path: '/Users/me/c' }, { machine_id: 'pc', path: 'C:\\y' }])];
    expect(recentFolders(sessions, projects, 'hub')).toEqual(['/Users/me/b', '/Users/me/a', '/Users/me/c']);
    expect(recentFolders(sessions, projects, 'hub', 1)).toEqual(['/Users/me/b']);
    expect(folderOn(projects[0], 'pc')).toBe('C:\\y');
    expect(folderOn(projects[0], 'lap')).toBeNull();
  });
});

describe('open sessions', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('keeps the most recent first and restores the newest that still exists', () => {
    expect(withOpened(['a', 'b', 'c'], 'b')).toEqual(['b', 'a', 'c']);
    expect(withOpened(Array.from({ length: 20 }, (_, i) => `s${i}`), 'new')).toHaveLength(12);
    const known = new Map([['b', {} as Session]]);
    expect(sessionToRestore(['gone', 'b'], known)).toBe('b');
    expect(sessionToRestore(['gone'], known)).toBeNull();
  });

  it('persists per browser and survives unavailable storage', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
    });
    rememberOpenSession('a');
    rememberOpenSession('b');
    expect(readOpenSessions()).toEqual(['b', 'a']);
    forgetOpenSession('b');
    expect(readOpenSessions()).toEqual(['a']);
    store.set('blirp.openSessions', 'not json');
    expect(readOpenSessions()).toEqual([]);

    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    });
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    expect(readOpenSessions()).toEqual([]);
    rememberOpenSession('x');
    warn.mockRestore();
  });
});
