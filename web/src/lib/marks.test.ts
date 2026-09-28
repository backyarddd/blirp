import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ProjectSummary } from './api/types.gen';
import { MARKS_MAX, markedFirst, parseMarks, projectKey, readMarks, sessionArchived, sessionKey, writeMarks } from './marks';

const g = globalThis as Record<string, unknown>;

afterEach(() => {
  delete g.localStorage;
  vi.restoreAllMocks();
});

describe('marks storage', () => {
  it('parses only lists of strings', () => {
    expect(parseMarks(null)).toEqual([]);
    expect(parseMarks('not json')).toEqual([]);
    expect(parseMarks('{"a":1}')).toEqual([]);
    expect(parseMarks('["s:a", 3, "p:b"]')).toEqual(['s:a', 'p:b']);
    const many = JSON.stringify(Array.from({ length: MARKS_MAX + 5 }, (_, i) => `s:${i}`));
    const kept = parseMarks(many);
    expect(kept).toHaveLength(MARKS_MAX);
    expect(kept[0]).toBe('s:5');
  });

  it('round-trips through storage and survives storage that throws', () => {
    const store = new Map<string, string>();
    g.localStorage = { getItem: (k: string) => store.get(k) ?? null, setItem: (k: string, v: string) => void store.set(k, v) };
    writeMarks('pinned', new Set(['s:a', 'p:b']));
    expect(readMarks('pinned')).toEqual(['s:a', 'p:b']);
    expect(readMarks('archived')).toEqual([]);

    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    g.localStorage = {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    };
    expect(readMarks('pinned')).toEqual([]);
    expect(() => writeMarks('pinned', ['s:a'])).not.toThrow();
    expect(warn).toHaveBeenCalled();
  });
});

describe('marks', () => {
  it('keys every Chats bucket as one group', () => {
    expect(sessionKey('x')).toBe('s:x');
    expect(projectKey({ id: 'p1' })).toBe('p:p1');
    expect(projectKey({ id: 'chats-m1', chats: true })).toBe('p:chats');
    expect(projectKey({ id: 'chats-m2', chats: true })).toBe('p:chats');
  });

  it('puts marked items first, keeping the order within each part', () => {
    expect(markedFirst([1, 2, 3, 4, 5], (n) => n % 2 === 0)).toEqual([2, 4, 1, 3, 5]);
  });

  it('archives a session by its own mark or its project', () => {
    const base: ProjectSummary = {
      id: 'p',
      name: 'P',
      created_at: 0,
      updated_at: 0,
      deleted: false,
      chats: false,
      merged_into: null,
      paths: [],
      is_git: false,
      is_home: false,
      workspace: null,
      session_count: 0,
      live_session_count: 0,
      last_activity_at: null,
    };
    const projects = new Map<string, ProjectSummary>([
      ['p', base],
      ['chats-m', { ...base, id: 'chats-m', chats: true }],
    ]);
    expect(sessionArchived({ id: 'a', project_id: 'p' }, new Set(['s:a']), projects)).toBe(true);
    expect(sessionArchived({ id: 'a', project_id: 'p' }, new Set(['p:p']), projects)).toBe(true);
    expect(sessionArchived({ id: 'a', project_id: 'chats-m' }, new Set(['p:chats']), projects)).toBe(true);
    expect(sessionArchived({ id: 'a', project_id: 'p' }, new Set(['s:b', 'p:q']), projects)).toBe(false);
  });
});
