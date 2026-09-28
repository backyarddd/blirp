import { describe, expect, it, vi } from 'vitest';
import type { ProjectSummary, Record as MemoryRecord, Session } from './api/types.gen';
import {
  bulkSummary,
  folderActions,
  projectActions,
  projectPath,
  recordActions,
  resumeCommand,
  sessionActions,
  trashActions,
  type ActionEnv,
  type ProjectOps,
  type RecordOps,
  type SessionOps,
} from './actions';
import { actionsOf, isSeparator, SEPARATOR, tidy, type MenuItem } from './menu';

const session = (over: Partial<Session> = {}): Session => ({
  id: 's1',
  project_id: 'p',
  machine_id: 'me',
  agent: 'claude',
  agent_session_id: 'u1',
  origin: 'blirp',
  cwd: '/w',
  title: null,
  status: 'completed',
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
  title_updated_at: 0,
  project_updated_at: 0,
  compacted_at: null,
  context_near_full_at: null,
  ...over,
});

const project = (over: Partial<ProjectSummary> = {}): ProjectSummary => ({
  id: 'p',
  name: 'Proj',
  created_at: 0,
  updated_at: 0,
  deleted: false,
  chats: false,
  merged_into: null,
  paths: [{ machine_id: 'me', path: '/w', git_remote: null, local: true, is_git: false }],
  is_git: false,
  is_home: false,
  workspace: null,
  session_count: 1,
  live_session_count: 0,
  last_activity_at: null,
  ...over,
});

const env = (over: Partial<ActionEnv> = {}): ActionEnv => ({
  control: true,
  admin: true,
  selfId: 'me',
  agents: [
    { id: 'claude', resume_command: ['claude', '--resume'] },
    { id: 'shell', resume_command: null },
  ],
  pinned: new Set(),
  archived: new Set(),
  handingOff: () => false,
  chatCandidate: () => false,
  ...over,
});

const sessionOps = (): SessionOps => ({
  rename: vi.fn(),
  move: vi.fn(),
  setPinned: vi.fn(),
  setArchived: vi.fn(),
  stop: vi.fn(),
  resume: vi.fn(),
  fork: vi.fn(),
  continueIn: vi.fn(),
  terminalHere: vi.fn(),
  open: vi.fn(),
  copy: vi.fn(),
  removeWorktree: vi.fn(),
  remove: vi.fn(),
  stopAndRemove: vi.fn(),
});

const projectOps = (): ProjectOps => ({
  newSession: vi.fn(),
  rename: vi.fn(),
  setPinned: vi.fn(),
  setArchived: vi.fn(),
  addFolder: vi.fn(),
  open: vi.fn(),
  terminalHere: vi.fn(),
  copy: vi.fn(),
  merge: vi.fn(),
  toChats: vi.fn(),
  remove: vi.fn(),
  removeFolder: vi.fn(),
  restore: vi.fn(),
});

const labels = (items: MenuItem[]): string[] => actionsOf(items).map((a) => a.label);
const item = (items: MenuItem[], label: string) => actionsOf(items).find((a) => a.label === label);

describe('session actions', () => {
  it('offers everything for an ended local session on the local client', () => {
    expect(labels(sessionActions(session({ worktree: '/wt' }), env(), sessionOps()))).toEqual([
      'Rename…',
      'Move…',
      'Pin',
      'Archive',
      'Resume',
      'Start new session from this session',
      'Continue in…',
      'Open terminal here',
      'Open folder',
      'Open in editor',
      'Copy session id',
      'Copy resume command',
      'Copy folder path',
      'Remove worktree…',
      'Delete…',
    ]);
  });

  it('offers stop and stop-and-delete for a running blirp session, never plain delete', () => {
    const l = labels(sessionActions(session({ status: 'working', worktree: '/wt' }), env(), sessionOps()));
    expect(l).toContain('Stop');
    expect(l).toContain('Stop and delete…');
    expect(l).not.toContain('Delete…');
    expect(l).not.toContain('Resume');
    expect(l).not.toContain('Remove worktree…');
  });

  it('cannot stop or delete a running session started outside blirp', () => {
    const l = labels(sessionActions(session({ status: 'working', origin: 'external' }), env(), sessionOps()));
    expect(l).not.toContain('Stop');
    expect(l).not.toContain('Stop and delete…');
    expect(l).not.toContain('Delete…');
  });

  it('keeps a view-only device to local marks and copying', () => {
    expect(labels(sessionActions(session(), env({ control: false, admin: false }), sessionOps()))).toEqual([
      'Pin',
      'Archive',
      'Copy session id',
      'Copy resume command',
      'Copy folder path',
    ]);
  });

  it('opens folders only for admins and only for sessions on this machine', () => {
    for (const [e, s] of [
      [env({ admin: false }), session()],
      [env(), session({ machine_id: 'other' })],
      [env({ selfId: null }), session()],
    ] as const) {
      const l = labels(sessionActions(s, e, sessionOps()));
      expect(l).not.toContain('Open folder');
      expect(l).not.toContain('Open in editor');
    }
    // Another machine's worktree is removed there, not from here.
    expect(labels(sessionActions(session({ machine_id: 'other', worktree: '/wt' }), env(), sessionOps()))).not.toContain(
      'Remove worktree…',
    );
  });

  it('leaves out moving, pinning and archiving a subagent session', () => {
    const l = labels(sessionActions(session({ origin: 'external', parent_session_id: 'parent' }), env(), sessionOps()));
    expect(l).not.toContain('Move…');
    expect(l).not.toContain('Pin');
    expect(l).not.toContain('Archive');
    expect(l).toContain('Rename…');
  });

  it('reflects pin and archive state and passes the new state on', () => {
    const ops = sessionOps();
    const items = sessionActions(session(), env({ pinned: new Set(['s:s1']), archived: new Set(['s:s1']) }), ops);
    item(items, 'Unpin')?.onselect();
    item(items, 'Unarchive')?.onselect();
    expect(ops.setPinned).toHaveBeenCalledWith(expect.objectContaining({ id: 's1' }), false);
    expect(ops.setArchived).toHaveBeenCalledWith(expect.objectContaining({ id: 's1' }), false);
  });

  it('disables starting from a session while a handoff from it is prepared', () => {
    const items = sessionActions(session(), env({ handingOff: (id) => id === 's1' }), sessionOps());
    expect(item(items, 'Start new session from this session')?.disabled).toBe(true);
    expect(item(items, 'Continue in…')?.disabled).toBe(true);
  });

  it('has no stray separators and marks deletes as dangerous', () => {
    const items = sessionActions(session(), env({ control: false }), sessionOps());
    const [first] = items;
    const last = items.at(-1);
    expect(first !== undefined && !isSeparator(first)).toBe(true);
    expect(last !== undefined && !isSeparator(last)).toBe(true);
    expect(item(sessionActions(session(), env(), sessionOps()), 'Delete…')?.danger).toBe(true);
  });
});

describe('resume command', () => {
  it('needs an agent with id-based resume and the agent session id', () => {
    const agents = env().agents;
    expect(resumeCommand({ agent: 'claude', agent_session_id: 'u1' }, agents)).toBe('claude --resume u1');
    expect(resumeCommand({ agent: 'claude', agent_session_id: null }, agents)).toBeNull();
    expect(resumeCommand({ agent: 'shell', agent_session_id: 'x' }, agents)).toBeNull();
    expect(resumeCommand({ agent: 'unknown', agent_session_id: 'x' }, agents)).toBeNull();
    expect(labels(sessionActions(session({ agent_session_id: null }), env(), sessionOps()))).not.toContain('Copy resume command');
  });
});

describe('project actions', () => {
  it('gates delete and merge on admin, the rest on control', () => {
    const control = labels(projectActions(project(), env({ admin: false }), projectOps()));
    expect(control).not.toContain('Delete…');
    expect(control).not.toContain('Merge into…');
    expect(control).not.toContain('Open folder');
    expect(control).toContain('Rename…');
    expect(control).toContain('Add folder…');
    const admin = labels(projectActions(project(), env(), projectOps()));
    expect(admin).toContain('Delete…');
    expect(admin).toContain('Merge into…');
    expect(admin).toContain('Open folder');
    expect(labels(projectActions(project(), env({ control: false, admin: false }), projectOps()))).toEqual([
      'Pin',
      'Archive',
      'Copy folder path',
      'Copy project id',
    ]);
  });

  it('never renames, archives, merges or deletes Chats', () => {
    const items = projectActions(project({ chats: true, paths: [] }), env(), projectOps());
    expect(item(items, 'Rename…')?.disabled).toBe(true);
    expect(labels(items)).toEqual(['New chat', 'Rename…', 'Pin']);
  });

  it('offers Move to Chats only for a candidate', () => {
    expect(labels(projectActions(project(), env(), projectOps()))).not.toContain('Move to Chats…');
    expect(labels(projectActions(project(), env({ chatCandidate: (id) => id === 'p' }), projectOps()))).toContain('Move to Chats…');
  });

  it('opens the local folder, else the workspace, and nothing without either', () => {
    const ops = projectOps();
    const withWorkspace = project({ paths: [{ machine_id: 'other', path: '/o', git_remote: null, local: false, is_git: false }], workspace: '/ws' });
    expect(projectPath(withWorkspace)).toBe('/ws');
    item(projectActions(withWorkspace, env(), ops), 'Open folder')?.onselect();
    expect(ops.open).toHaveBeenCalledWith(withWorkspace, 'folder', '/ws');
    const elsewhere = project({ paths: [{ machine_id: 'other', path: '/o', git_remote: null, local: false, is_git: false }] });
    expect(labels(projectActions(elsewhere, env(), ops))).not.toContain('Open folder');
    expect(labels(projectActions(elsewhere, env(), ops))).not.toContain('Copy folder path');
  });

  it('folder rows remove folders with control, open them with admin', () => {
    expect(labels(folderActions(project(), '/w', env(), projectOps()))).toEqual([
      'Open folder',
      'Open in editor',
      'Open terminal here',
      'Copy folder path',
      'Remove folder…',
    ]);
    expect(labels(folderActions(project(), '/w', env({ control: false, admin: false }), projectOps()))).toEqual(['Copy folder path']);
  });

  it('restores from the Trash only as admin', () => {
    expect(labels(trashActions(project(), env(), projectOps()))).toEqual(['Restore', 'Copy project id']);
    expect(labels(trashActions(project(), env({ admin: false }), projectOps()))).toEqual(['Copy project id']);
  });
});

describe('record actions', () => {
  const record = (over: Partial<MemoryRecord> = {}): MemoryRecord => ({
    id: 'r1',
    project_id: 'p',
    kind: 'note',
    title: 'T',
    body: '',
    status: 'active',
    pinned: false,
    source_session_id: null,
    created_at: 0,
    updated_at: 0,
    updated_by: 'user',
    ...over,
  });
  const ops = (): RecordOps => ({ edit: vi.fn(), setPinned: vi.fn(), setStatus: vi.fn(), move: vi.fn(), remove: vi.fn() });

  it('offers resolve and archive for an active record, reopen for a resolved one, unarchive for an archived one', () => {
    expect(labels(recordActions(record(), env(), ops()))).toEqual([
      'Edit…',
      'Pin',
      'Mark resolved',
      'Archive',
      'Move to project…',
      'Delete…',
    ]);
    expect(labels(recordActions(record({ status: 'resolved', pinned: true }), env(), ops()))).toEqual([
      'Edit…',
      'Unpin',
      'Reopen',
      'Archive',
      'Move to project…',
      'Delete…',
    ]);
    const o = ops();
    const archived = recordActions(record({ status: 'archived' }), env(), o);
    expect(labels(archived)).toEqual(['Edit…', 'Pin', 'Unarchive', 'Move to project…', 'Delete…']);
    item(archived, 'Unarchive')?.onselect();
    expect(o.setStatus).toHaveBeenCalledWith(expect.objectContaining({ id: 'r1' }), 'active');
    expect(item(archived, 'Delete…')?.danger).toBe(true);
  });

  it('offers nothing without control', () => {
    expect(recordActions(record(), env({ control: false }), ops())).toEqual([]);
  });
});

describe('menus', () => {
  it('tidies separators left by gated items', () => {
    const a = { label: 'a', onselect: () => {} };
    const b = { label: 'b', onselect: () => {} };
    expect(tidy([SEPARATOR, a, SEPARATOR, SEPARATOR, b, SEPARATOR])).toEqual([a, SEPARATOR, b]);
    expect(tidy([SEPARATOR, SEPARATOR])).toEqual([]);
  });
});

describe('bulk summary', () => {
  it('counts what worked and names what did not', () => {
    expect(bulkSummary('Deleted', 'Not deleted', 3, [])).toBe('Deleted 3 sessions.');
    expect(bulkSummary('Moved', 'Not moved', 1, [], ' to Chats')).toBe('Moved 1 session to Chats.');
    expect(bulkSummary('Deleted', 'Not deleted', 3, [{ title: 'a', reason: 'it is running; stop it first' }])).toBe(
      'Deleted 2 of 3 sessions. Not deleted: "a" (it is running; stop it first).',
    );
    const f = (t: string) => ({ title: t, reason: 'laptop is offline' });
    expect(bulkSummary('Moved', 'Not moved', 5, [f('a'), f('b'), f('c'), f('d'), f('e')], ' to "X"')).toBe(
      'No sessions moved to "X". Not moved: "a" (laptop is offline); "b" (laptop is offline); "c" (laptop is offline); and 2 more.',
    );
    expect(bulkSummary('Archived', 'Not archived', 2, [], '', ['record', 'records'])).toBe('Archived 2 records.');
  });
});
