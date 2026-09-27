import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { LaunchSession, Session, SessionsPage } from './api/types.gen';
import type { SessionQuery } from './api/client';

// The store reads the browser's location and storage when it is created.
vi.hoisted(() => {
  const noop = (): void => undefined;
  const g = globalThis as Record<string, unknown>;
  g.location = { pathname: '/', search: '', origin: 'http://127.0.0.1', protocol: 'http:', host: '127.0.0.1' };
  g.window = { innerWidth: 1280, addEventListener: noop, removeEventListener: noop, scrollTo: noop };
  g.document = { addEventListener: noop, removeEventListener: noop };
  g.localStorage = { getItem: () => null, setItem: noop, removeItem: noop };
  g.history = { pushState: noop, replaceState: noop };
});

const list = vi.hoisted(() => vi.fn<(q?: SessionQuery) => Promise<SessionsPage>>());
const launch = vi.hoisted(() => vi.fn<(req: LaunchSession) => Promise<Session>>());
vi.mock('./api/client', async (orig) => {
  const real = await orig<typeof import('./api/client')>();
  return { ...real, api: { ...real.api, sessions: { ...real.api.sessions, list, launch } } };
});

const { app } = await import('./app.svelte');
const { ApiError } = await import('./api/client');

const mk = (id: string, last: number): Session => ({
  id,
  project_id: 'p',
  machine_id: 'm',
  agent: 'shell',
  agent_session_id: null,
  origin: 'blirp',
  cwd: '/w',
  title: null,
  status: 'completed',
  branch: null,
  worktree: null,
  transcript_path: null,
  started_at: 0,
  ended_at: null,
  last_activity_at: last,
  exit_code: null,
  summary: null,
  distilled_through_seq: 0,
  tokens_in: 0,
  tokens_out: 0,
  cost_usd: 0,
  parent_session_id: null,
  stopped_by_user: false,
  title_updated_at: 0,
  project_updated_at: 0,
  compacted_at: null,
  context_near_full_at: null,
});

function deferred<T>(): { promise: Promise<T>; resolve: (v: T) => void } {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

const ids = (): string[] => app.sessions.map((s) => s.id);

describe('loading more sessions', () => {
  beforeEach(async () => {
    list.mockReset();
    list.mockResolvedValueOnce({ items: [mk('a', 30), mk('b', 20)], next_cursor: 'c1' });
    await app.refreshSessions();
  });

  it('appends the next page once, in list order, and drops sessions it already has', async () => {
    // "b" moved behind the cursor between pages and comes back; it stays once.
    list.mockResolvedValueOnce({ items: [mk('b', 20), mk('c', 10)], next_cursor: null });
    await app.loadMoreSessions();
    expect(list).toHaveBeenLastCalledWith(expect.objectContaining({ cursor: 'c1' }));
    expect(ids()).toEqual(['a', 'b', 'c']);
    expect(app.sessionsCursor).toBeNull();
    // Nothing more to load: no request.
    await app.loadMoreSessions();
    expect(list).toHaveBeenCalledTimes(2);
  });

  it('drops a page that arrives after a refresh started the list over', async () => {
    const late = deferred<SessionsPage>();
    list.mockReturnValueOnce(late.promise);
    const loading = app.loadMoreSessions();
    list.mockResolvedValueOnce({ items: [mk('fresh', 40)], next_cursor: 'c2' });
    await app.refreshSessions();
    late.resolve({ items: [mk('stale', 1)], next_cursor: 'c-old' });
    await loading;
    expect(ids()).toEqual(['fresh']);
    expect(app.sessionsCursor).toBe('c2');
    expect(app.sessionsLoadingMore).toBe(false);
  });

  it('keeps the list and its cursor when a page fails', async () => {
    list.mockRejectedValueOnce(new Error('offline'));
    await app.loadMoreSessions();
    expect(ids()).toEqual(['a', 'b']);
    expect(app.sessionsCursor).toBe('c1');
    expect(app.toasts.at(-1)?.text).toContain('Could not load more sessions');
  });
});

describe('starting a session from another one', () => {
  it('marks the source while the daemon prepares the handoff and never starts it twice', async () => {
    const reply = deferred<Session>();
    launch.mockReset();
    launch.mockReturnValueOnce(reply.promise);
    const first = app.launch({ continue_from: 'src', agent: 'claude' });
    expect(app.handoffFrom.has('src')).toBe(true);
    // A second click while the first handoff is pending does nothing.
    expect(await app.launch({ continue_from: 'src', agent: 'codex' })).toBeUndefined();
    expect(launch).toHaveBeenCalledTimes(1);
    reply.resolve(mk('new', 50));
    expect((await first)?.id).toBe('new');
    expect(app.handoffFrom.has('src')).toBe(false);
    // A failed launch clears the mark too.
    launch.mockRejectedValueOnce(new Error('agent not installed'));
    expect(await app.launch({ continue_from: 'src', agent: 'claude' })).toBeUndefined();
    expect(app.handoffFrom.has('src')).toBe(false);
    // The daemon refuses a second handoff started elsewhere: explained, not an error.
    launch.mockRejectedValueOnce(new ApiError(409, 'handoff_in_progress', 'already being started'));
    expect(await app.launch({ continue_from: 'src', agent: 'claude' })).toBeUndefined();
    expect(app.toasts.at(-1)).toMatchObject({ kind: 'info', text: expect.stringContaining('already being started') });
  });
});
