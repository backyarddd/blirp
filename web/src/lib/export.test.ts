import { describe, expect, it, vi } from 'vitest';
import type { Event, EventsPage, ProjectSummary, Record as MemoryRecord, Session } from './api/types.gen';
import { allEvents, exportName, fence, memoryJson, memoryMarkdown, transcriptJson, transcriptMarkdown } from './export';

const session: Session = {
  id: 's1',
  project_id: 'p1',
  machine_id: 'm',
  agent: 'claude',
  agent_session_id: null,
  origin: 'blirp',
  cwd: '/work/app',
  title: 'Fix the build',
  status: 'completed',
  branch: null,
  worktree: null,
  transcript_path: null,
  started_at: Date.UTC(2026, 8, 28, 10, 0, 0),
  ended_at: Date.UTC(2026, 8, 28, 11, 0, 0),
  last_activity_at: 0,
  exit_code: 0,
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
};

const event = (seq: number, kind: Event['kind'], text: string): Event => ({
  session_id: 's1',
  seq,
  ts: Date.UTC(2026, 8, 28, 10, seq, 0),
  kind,
  text,
  meta: null,
});

describe('allEvents', () => {
  it('reads every page, following next_after', async () => {
    const pages: Record<number, EventsPage> = {
      [-1]: { items: [event(0, 'user', 'a'), event(2, 'assistant', 'b')], next_after: 2 },
      2: { items: [event(3, 'user', 'c')], next_after: 3 },
      3: { items: [event(4, 'assistant', 'd')], next_after: null },
    };
    const page = vi.fn(async (after: number) => pages[after] ?? { items: [], next_after: null });
    expect((await allEvents(page)).map((e) => e.seq)).toEqual([0, 2, 3, 4]);
    expect(page.mock.calls.map((c) => c[0])).toEqual([-1, 2, 3]);
  });

  it('stops on a cursor that does not move on', async () => {
    const page = vi.fn(async () => ({ items: [event(0, 'user', 'a')], next_after: -1 }));
    expect(await allEvents(page)).toHaveLength(1);
    expect(page).toHaveBeenCalledTimes(1);
  });
});

describe('transcript', () => {
  it('has a header with title, agent, project, folder and dates, then each event', () => {
    const md = transcriptMarkdown(
      session,
      [event(1, 'user', 'Why does it fail?'), event(2, 'tool_result', 'error: ```x``` failed'), event(3, 'assistant', 'Fixed.')],
      'App',
    );
    expect(md).toContain('# Fix the build\n');
    expect(md).toContain('- Agent: Claude Code\n');
    expect(md).toContain('- Project: App\n');
    expect(md).toContain('- Folder: /work/app\n');
    expect(md).toContain('- Started: 2026-09-28T10:00:00.000Z\n');
    expect(md).toContain('- Ended: 2026-09-28T11:00:00.000Z\n');
    expect(md).toContain('## User (2026-09-28T10:01:00.000Z)\n\nWhy does it fail?\n');
    // Tool output is fenced with more backticks than it contains.
    expect(md).toContain('## Tool result (2026-09-28T10:02:00.000Z)\n\n````\nerror: ```x``` failed\n````\n');
    expect(md).toContain('## Assistant (2026-09-28T10:03:00.000Z)\n\nFixed.\n');
  });

  it('says when a session has not ended or has no events', () => {
    const md = transcriptMarkdown({ ...session, ended_at: null, title: null }, [], null);
    expect(md).toContain('# Claude Code in app\n');
    expect(md).toContain('- Ended: not ended\n');
    expect(md).not.toContain('- Project:');
    expect(md).toContain('_No transcript events._');
  });

  it('JSON is {session, events} with every event', () => {
    const parsed: unknown = JSON.parse(transcriptJson(session, [event(1, 'user', 'a')]));
    expect(parsed).toEqual({ session, events: [event(1, 'user', 'a')] });
  });

  it('fences outgrow the backticks inside', () => {
    expect(fence('plain')).toBe('```');
    expect(fence('a ````` b')).toBe('``````');
  });
});

describe('memory export', () => {
  const project: ProjectSummary = {
    id: 'p1',
    name: 'App',
    created_at: 1,
    updated_at: 2,
    deleted: false,
    chats: false,
    merged_into: null,
    paths: [],
    is_git: false,
    is_home: false,
    workspace: '/ws',
    session_count: 3,
    live_session_count: 0,
    last_activity_at: null,
  };
  const record: MemoryRecord = {
    id: 'r1',
    project_id: 'p1',
    kind: 'decision',
    title: 'Use SQLite',
    body: 'WAL mode.',
    status: 'active',
    pinned: true,
    source_session_id: null,
    created_at: 1,
    updated_at: Date.UTC(2026, 8, 1),
    updated_by: 'user',
  };

  it('JSON has the shape of `blirp mem brief --json`: the project row, the brief and the records', () => {
    const parsed: unknown = JSON.parse(memoryJson(project, null, [record]));
    expect(parsed).toEqual({
      project: { id: 'p1', name: 'App', created_at: 1, updated_at: 2, deleted: false, chats: false, merged_into: null },
      brief: null,
      records: [record],
    });
  });

  it('Markdown has the brief and each record with its kind', () => {
    const brief = { id: 'b', project_id: 'p1', body_md: 'An app.', version: 1, updated_at: 0, updated_by: 'user', machine_id: 'm' };
    const md = memoryMarkdown(project, brief, [record], Date.UTC(2026, 8, 28));
    expect(md).toContain('# App\n');
    expect(md).toContain('## Brief\n\nAn app.\n');
    expect(md).toContain('### Decision: Use SQLite (pinned)\n\nWAL mode.\n');
    expect(memoryMarkdown(project, null, [], 0)).toContain('_No brief yet._');
  });
});

describe('exportName', () => {
  it('makes a safe file name', () => {
    expect(exportName('Fix the build: v2!', 'md')).toBe('fix-the-build-v2.md');
    expect(exportName('日本語', 'json')).toBe('export.json');
  });
});
