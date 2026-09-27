import { describe, expect, it } from 'vitest';
import type { UpdateOutcome, UpdateStatus } from './api/types.gen';
import {
  DESKTOP_KEY,
  DISMISSED_KEY,
  PENDING_MS,
  bannerVersion,
  clearPending,
  compareVersions,
  desktopVersion,
  loadPending,
  readDismissed,
  restartStep,
  savePending,
  staleNotice,
  takeDesktopVersion,
  writeDismissed,
} from './update';

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
    installed: ok,
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

describe('after an update', () => {
  it('orders versions like semver', () => {
    expect(compareVersions('0.1.10', '0.1.9')).toBeGreaterThan(0);
    expect(compareVersions('0.2.0', '0.10.0')).toBeLessThan(0);
    expect(compareVersions('1.0.0', '1.0.0-rc.2')).toBeGreaterThan(0);
    expect(compareVersions('1.0.0-rc.1', '1.0.0-rc.2')).toBeLessThan(0);
    expect(compareVersions('v0.1.1', '0.1.1')).toBe(0);
    expect(compareVersions('0.1.1+build', '0.1.1')).toBe(0);
    expect(compareVersions('garbage', '0.1.1')).toBe(0);
  });

  it('offers a relaunch in the desktop app and a reload in a browser', () => {
    // Desktop shell older than the daemon: relaunch (that also loads the new UI).
    expect(staleNotice('0.2.0', '0.2.0', '0.1.1')).toBe('restart');
    expect(staleNotice('0.2.0', '0.1.1', '0.1.1')).toBe('restart');
    // Desktop shell current, page loaded from the old daemon.
    expect(staleNotice('0.2.0', '0.1.1', '0.2.0')).toBe('reload');
    // Browser tab (no shell version) loaded before the update.
    expect(staleNotice('0.2.0', '0.1.1', null)).toBe('reload');
    // Nothing newer, or health unknown.
    expect(staleNotice('0.2.0', '0.2.0', '0.2.0')).toBeNull();
    expect(staleNotice('0.2.0', '0.2.0', null)).toBeNull();
    expect(staleNotice('0.1.1', '0.2.0', '0.2.0')).toBeNull();
    expect(staleNotice(undefined, '0.1.1', '0.1.0')).toBeNull();
  });

  it('takes the desktop version out of the sign-in fragment', () => {
    const storage = memoryStorage();
    let url = '';
    const env = (hash: string) => ({
      location: { hash, pathname: '/sessions', search: '?q=1' },
      history: { state: null, replaceState: (_s: unknown, _t: string, u?: string | URL | null) => void (url = String(u)) },
      storage: () => storage,
    });
    takeDesktopVersion(env('#token=abc&app=0.1.1'));
    expect(url).toBe('/sessions?q=1#token=abc');
    expect(desktopVersion(() => storage)).toBe('0.1.1');
    // Not a version: dropped from the URL, not stored.
    storage.removeItem(DESKTOP_KEY);
    takeDesktopVersion(env('#app=%3Cscript%3E'));
    expect(url).toBe('/sessions?q=1');
    expect(desktopVersion(() => storage)).toBeNull();
    // No fragment parameter: nothing changes.
    url = 'untouched';
    takeDesktopVersion(env('#token=abc'));
    takeDesktopVersion(env(''));
    expect(url).toBe('untouched');
    const blocked = (): Storage => {
      throw new DOMException('denied', 'SecurityError');
    };
    expect(desktopVersion(blocked)).toBeNull();
    expect(() => takeDesktopVersion({ ...env('#app=0.1.1'), storage: blocked })).not.toThrow();
  });
});

describe('an update across the reload', () => {
  it('keeps the started update for this window until it is old', () => {
    const storage = memoryStorage();
    const s = (): Storage => storage;
    expect(loadPending(s)).toBeNull();
    savePending({ before: 5, target: '0.2.0', startedAt: 1000 }, s);
    expect(loadPending(s, 2000)).toEqual({ before: 5, target: '0.2.0', startedAt: 1000 });
    expect(loadPending(s, 1000 + PENDING_MS + 1)).toBeNull();
    savePending({ before: null, target: '0.2.0', startedAt: 1000 }, s);
    expect(loadPending(s, 2000)?.before).toBeNull();
    clearPending(s);
    expect(loadPending(s, 2000)).toBeNull();
    storage.setItem('blirp.update.pending', '{"before":"x","target":1}');
    expect(loadPending(s, 2000)).toBeNull();
    storage.setItem('blirp.update.pending', 'not json');
    expect(loadPending(s, 2000)).toBeNull();
    const blocked = (): Storage => {
      throw new DOMException('denied', 'SecurityError');
    };
    expect(loadPending(blocked)).toBeNull();
    expect(() => savePending({ before: null, target: '0.2.0', startedAt: 1 }, blocked)).not.toThrow();
  });

  it('shows a failure recorded after the reload, not an older one', () => {
    const failed = { from: '0.1.1', to: '0.2.0', installed: false, ok: false, error: 'checksum mismatch', finished_at: 9 };
    // The reloaded page asks the daemon (never "went down" from its point of view).
    expect(restartStep({ kind: 'status', status: status({ last_update: failed }) }, false, 5)).toEqual({ failed: 'checksum mismatch' });
    expect(restartStep({ kind: 'status', status: status({ last_update: failed }) }, false, 9)).toBe('wait');
    expect(restartStep({ kind: 'status', status: status({ last_update: { ...failed, ok: true, installed: true, error: null } }) }, false, 5)).toBe(
      'reload',
    );
  });
});
