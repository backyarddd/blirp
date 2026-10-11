import { afterEach, describe, expect, it, vi } from 'vitest';
import { PANES_MAX, closePane, followRoute, readPanes, splitWith, writePanes } from './panes';

const g = globalThis as Record<string, unknown>;

afterEach(() => {
  delete g.localStorage;
  vi.restoreAllMocks();
});

describe('splitWith', () => {
  it('starts the split with the current session and the target', () => {
    expect(splitWith([], 'a', 'b')).toEqual(['a', 'b']);
  });
  it('does not split without a current session or with itself', () => {
    expect(splitWith([], null, 'b')).toEqual([]);
    expect(splitWith([], 'a', 'a')).toEqual([]);
  });
  it('appends to a split', () => {
    expect(splitWith(['a', 'b'], 'b', 'c')).toEqual(['a', 'b', 'c']);
  });
  it('replaces the last pane of a full split', () => {
    const full = ['a', 'b', 'c', 'd'];
    expect(full.length).toBe(PANES_MAX);
    expect(splitWith(full, 'a', 'e')).toEqual(['a', 'b', 'c', 'e']);
  });
  it('leaves a session that is already a pane', () => {
    expect(splitWith(['a', 'b'], 'a', 'b')).toEqual(['a', 'b']);
  });
});

describe('followRoute', () => {
  it('replaces the focused pane', () => {
    expect(followRoute(['a', 'b', 'c'], 'b', 'x')).toEqual(['a', 'x', 'c']);
  });
  it('replaces the last pane when the focused one is unknown', () => {
    expect(followRoute(['a', 'b'], 'zz', 'x')).toEqual(['a', 'x']);
    expect(followRoute(['a', 'b'], null, 'x')).toEqual(['a', 'x']);
  });
  it('leaves the panes when the next session is already one', () => {
    expect(followRoute(['a', 'b'], 'a', 'b')).toEqual(['a', 'b']);
  });
  it('does nothing without a split', () => {
    expect(followRoute([], null, 'x')).toEqual([]);
    expect(followRoute(['a'], 'a', 'x')).toEqual(['a']);
  });
});

describe('closePane', () => {
  it('keeps the rest while two or more remain', () => {
    expect(closePane(['a', 'b', 'c'], 'b')).toEqual(['a', 'c']);
  });
  it('ends the split when one is left', () => {
    expect(closePane(['a', 'b'], 'a')).toEqual([]);
  });
});

describe('pane storage', () => {
  it('round-trips and reads a single pane or garbage as no split', () => {
    const store = new Map<string, string>();
    g.localStorage = { getItem: (k: string) => store.get(k) ?? null, setItem: (k: string, v: string) => void store.set(k, v) };
    expect(readPanes()).toEqual([]);
    writePanes(['a', 'b', 'c']);
    expect(readPanes()).toEqual(['a', 'b', 'c']);
    writePanes(['a']);
    expect(readPanes()).toEqual([]);
    store.set('blirp.panes', 'not json');
    expect(readPanes()).toEqual([]);
    writePanes(['a', 'b', 'c', 'd', 'e']);
    expect(readPanes()).toEqual(['a', 'b', 'c', 'd']);
  });
  it('survives storage that throws', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    g.localStorage = {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    };
    expect(readPanes()).toEqual([]);
    expect(() => writePanes(['a', 'b'])).not.toThrow();
  });
});
