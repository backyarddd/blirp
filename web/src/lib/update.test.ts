import { describe, expect, it } from 'vitest';
import type { UpdateOutcome, UpdateStatus } from './api/types.gen';
import { DISMISSED_KEY, bannerVersion, readDismissed, restartStep, writeDismissed } from './update';

function status(over: Partial<UpdateStatus> = {}): UpdateStatus {
  return {
    current: '0.1.1',
    latest: '0.2.0',
    available: true,
    notes_url: 'https://example.invalid/v0.2.0',
    enabled: true,
    self_update: true,
    checked_at: 1,
    error: null,
    last_update: null,
    ...over,
  };
}

function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    get length() {
      return m.size;
    },
    clear: () => m.clear(),
    getItem: (k) => m.get(k) ?? null,
    key: (i) => [...m.keys()][i] ?? null,
    removeItem: (k) => void m.delete(k),
    setItem: (k, v) => void m.set(k, v),
  };
}

describe('update banner', () => {
  it('shows an available release until that version is dismissed', () => {
    expect(bannerVersion(status(), null)).toBe('0.2.0');
    expect(bannerVersion(status(), '0.2.0')).toBeNull();
    // A newer release than the dismissed one shows again.
    expect(bannerVersion(status({ latest: '0.3.0' }), '0.2.0')).toBe('0.3.0');
  });

  it('stays hidden without an update', () => {
    expect(bannerVersion(null, null)).toBeNull();
    expect(bannerVersion(status({ available: false }), null)).toBeNull();
    expect(bannerVersion(status({ latest: null, available: false, error: 'offline' }), null)).toBeNull();
  });

  it('remembers the dismissal and survives blocked storage', () => {
    const storage = memoryStorage();
    expect(readDismissed(() => storage)).toBeNull();
    writeDismissed('0.2.0', () => storage);
    expect(storage.getItem(DISMISSED_KEY)).toBe('0.2.0');
    expect(readDismissed(() => storage)).toBe('0.2.0');
    const blocked = (): Storage => {
      throw new DOMException('denied', 'SecurityError');
    };
    expect(readDismissed(blocked)).toBeNull();
    expect(() => writeDismissed('0.2.0', blocked)).not.toThrow();
    expect(readDismissed(() => null)).toBeNull();
  });
});

describe('waiting for the restart', () => {
  const outcome = (ok: boolean, finished_at: number): UpdateOutcome => ({
    from: '0.1.1',
    to: '0.2.0',
    ok,
    error: ok ? null : 'checksum mismatch',
    finished_at,
  });

  it('waits while the old daemon still answers or none does', () => {
    expect(restartStep({ kind: 'unreachable' }, false, null)).toBe('wait');
    expect(restartStep({ kind: 'unreachable' }, true, null)).toBe('wait');
    expect(restartStep({ kind: 'status', status: status() }, false, null)).toBe('wait');
    // An older attempt in the log is not this one.
    expect(restartStep({ kind: 'status', status: status({ last_update: outcome(false, 5) }) }, false, 5)).toBe('wait');
  });

  it('reloads once the restarted daemon answers', () => {
    expect(restartStep({ kind: 'unauthorized' }, false, null)).toBe('reload');
    expect(restartStep({ kind: 'status', status: status() }, true, null)).toBe('reload');
  });

  it('reports a failure that left the daemon running', () => {
    expect(restartStep({ kind: 'status', status: status({ last_update: outcome(false, 9) }) }, false, 5)).toEqual({
      failed: 'checksum mismatch',
    });
  });
});
