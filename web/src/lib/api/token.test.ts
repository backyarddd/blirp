import { describe, expect, it, vi } from 'vitest';
import { authToken, bootstrapToken, type TokenEnv } from './token';

const TOKEN = 'ab'.repeat(32);

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

function env(hash: string, storage: () => Storage | null): TokenEnv & { replaced: string[] } {
  const replaced: string[] = [];
  return {
    replaced,
    location: { hash, pathname: '/settings/sync', search: '?join=x' },
    history: { state: null, replaceState: (_s: unknown, _t: string, url?: string | URL | null) => void replaced.push(String(url)) },
    storage,
  };
}

// Module state (the in-memory fallback) is per file; each test uses its own storage.
describe('token bootstrap', () => {
  it('moves the fragment token into storage and strips it from the URL', () => {
    const storage = memoryStorage();
    const e = env(`#token=${TOKEN}&x=1`, () => storage);
    expect(bootstrapToken(e)).toBe(true);
    expect(storage.getItem('blirp.token')).toBe(TOKEN);
    expect(e.replaced).toEqual(['/settings/sync?join=x#x=1']);
    expect(authToken({ storage: () => storage })).toBe(TOKEN);
  });

  it('strips the fragment entirely when only the token was there', () => {
    const e = env(`#token=${TOKEN}`, () => memoryStorage());
    bootstrapToken(e);
    expect(e.replaced).toEqual(['/settings/sync?join=x']);
  });

  it('leaves URLs without a token alone', () => {
    for (const hash of ['', '#', '#section']) {
      const e = env(hash, () => memoryStorage());
      expect(bootstrapToken(e)).toBe(false);
      expect(e.replaced).toEqual([]);
    }
  });

  it('strips but never stores a malformed token', () => {
    const storage = memoryStorage();
    const e = env('#token=abc%0D%0AX-Evil:1', () => storage);
    expect(bootstrapToken(e)).toBe(false);
    expect(e.replaced).toHaveLength(1);
    expect(storage.getItem('blirp.token')).toBeNull();
    storage.setItem('blirp.token', 'not-a-token');
    // The in-memory copy from the first test is the fallback, never the bad stored value.
    expect(authToken({ storage: () => storage })).toBe(TOKEN);
  });

  it('keeps working in memory when storage throws', () => {
    const blocked = (): Storage => {
      throw new DOMException('blocked', 'SecurityError');
    };
    const other = 'cd'.repeat(32);
    bootstrapToken(env(`#token=${other}`, blocked));
    expect(authToken({ storage: blocked })).toBe(other);
    const failing = memoryStorage();
    failing.setItem = vi.fn(() => {
      throw new DOMException('full', 'QuotaExceededError');
    });
    expect(() => bootstrapToken(env(`#token=${TOKEN}`, () => failing))).not.toThrow();
    expect(authToken({ storage: () => failing })).toBe(TOKEN);
  });

  it('prefers a newer token another tab stored', () => {
    const storage = memoryStorage();
    const newer = 'ef'.repeat(32);
    storage.setItem('blirp.token', newer);
    expect(authToken({ storage: () => storage })).toBe(newer);
  });
});
