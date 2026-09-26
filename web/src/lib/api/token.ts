// Local sign-in (ARCHITECTURE §11). The desktop app and `blirp open` load `/#token=<runtime token>`:
// a fragment is never sent to a server or written to its logs. The token is kept in this origin's
// localStorage (the origin includes the port, so other local servers cannot read it; cookies would
// reach every server on 127.0.0.1) and sent as `Authorization: Bearer`. The LAN portal has no token
// and authenticates with its HttpOnly device cookie instead.

const KEY = 'blirp.token';
/** Tokens are 64 hex characters; anything else is not ours and never becomes a header. */
const TOKEN = /^[0-9a-f]{64}$/i;

/** Fallback when storage is unavailable (blocked, private mode): this page only. */
let memory: string | null = null;

export interface TokenEnv {
  location: Pick<Location, 'hash' | 'pathname' | 'search'>;
  history: Pick<History, 'replaceState' | 'state'>;
  storage: () => Storage | null;
}

function browserStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const browserEnv = (): TokenEnv => ({ location, history, storage: browserStorage });

/**
 * Takes `token` out of the URL fragment (other fragment parameters stay), stores it and replaces
 * the history entry so the token does not stay in the address bar or the back button. Call once,
 * before the first request.
 */
export function bootstrapToken(env: TokenEnv = browserEnv()): void {
  const { hash, pathname, search } = env.location;
  if (!hash.startsWith('#')) return;
  const params = new URLSearchParams(hash.slice(1));
  const token = params.get('token');
  if (token === null) return;
  params.delete('token');
  const rest = params.toString();
  env.history.replaceState(env.history.state, '', `${pathname}${search}${rest ? `#${rest}` : ''}`);
  if (!TOKEN.test(token)) return;
  memory = token;
  try {
    env.storage()?.setItem(KEY, token);
  } catch {
    // Quota or blocked storage: the in-memory copy serves this page.
  }
}

/** The runtime token of this origin, or null (portal pages, not signed in). */
export function authToken(env: Pick<TokenEnv, 'storage'> = { storage: browserStorage }): string | null {
  // Storage first: another tab may have stored a newer token (the daemon restarted).
  try {
    const stored = env.storage()?.getItem(KEY) ?? null;
    if (stored !== null && TOKEN.test(stored)) return stored;
  } catch {
    // Blocked storage: fall back to this page's copy.
  }
  return memory;
}
