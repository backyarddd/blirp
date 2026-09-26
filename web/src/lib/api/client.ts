// The only module that talks HTTP to the daemon. Everything else imports `api`.
import type {
  AgentInfo,
  Brief,
  BrowserInvite,
  CloneJob,
  CloneRepo,
  CreateRecord,
  CreateResource,
  CreateWikiPage,
  Device,
  DirListing,
  EventsPage,
  FileContent,
  GitDiff,
  GitStatus,
  Health,
  Injection,
  JoinHub,
  JoinPreview,
  LaunchSession,
  Machine,
  MachineDirs,
  OpenTarget,
  PatchDevice,
  PatchRecord,
  PatchResource,
  ProjectMemory,
  ProjectSummary,
  PutWikiPage,
  Record as MemoryRecord,
  Resource,
  RevertBrief,
  SearchHitKind,
  SearchResults,
  Session,
  SessionDetail,
  SessionStatus,
  SessionsPage,
  SettingsPatch,
  SettingsView,
  Suggestion,
  SuggestionStatus,
  SyncInvite,
  SyncStatus,
  UpdateStatus,
  WikiPage,
  WsTicket,
} from './types.gen';
import { authToken } from './token';

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }

  get unauthorized(): boolean {
    return this.status === 401;
  }
}

type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
type Query = Record<string, string | number | boolean | undefined | null>;

let unauthorizedHandler: (() => void) | null = null;

/** Called once per 401 so the app can switch to the "open from desktop" screen. */
export function onUnauthorized(handler: () => void): void {
  unauthorizedHandler = handler;
}

export function buildUrl(path: string, query?: Query): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null || v === '') continue;
    params.set(k, String(v));
  }
  const qs = params.toString();
  return qs ? `${path}?${qs}` : path;
}

function isErrorBody(v: unknown): v is { error: { code: string; message: string } } {
  if (typeof v !== 'object' || v === null || !('error' in v)) return false;
  const e = (v as { error: unknown }).error;
  return (
    typeof e === 'object' &&
    e !== null &&
    typeof (e as { code?: unknown }).code === 'string' &&
    typeof (e as { message?: unknown }).message === 'string'
  );
}

async function errorFrom(res: Response): Promise<ApiError> {
  const text = await res.text().catch(() => '');
  if (text) {
    try {
      const parsed: unknown = JSON.parse(text);
      if (isErrorBody(parsed)) return new ApiError(res.status, parsed.error.code, parsed.error.message);
    } catch {
      // Not JSON (e.g. a proxy error page); fall through to the raw text.
    }
  }
  const message = text.trim().slice(0, 300) || res.statusText || `HTTP ${res.status}`;
  return new ApiError(res.status, `http_${res.status}`, message);
}

export async function request<T>(method: Method, path: string, body?: unknown, query?: Query): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' };
  // Local pages carry the runtime token; portal pages rely on their same-origin device cookie.
  const token = authToken();
  if (token !== null) headers.Authorization = `Bearer ${token}`;
  const init: RequestInit = { method, credentials: 'same-origin', headers };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let res: Response;
  try {
    res = await fetch(buildUrl(path, query), init);
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    throw new ApiError(0, 'network', `Cannot reach the blirp daemon (${detail})`);
  }
  if (!res.ok) {
    const err = await errorFrom(res);
    if (err.unauthorized) unauthorizedHandler?.();
    throw err;
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (!text) return undefined as T;
  try {
    // Trust boundary note: the daemon is same-origin and its DTOs are generated from Rust;
    // shape validation happens there.
    return JSON.parse(text) as T;
  } catch {
    throw new ApiError(res.status, 'bad_json', 'The daemon returned an invalid response');
  }
}

export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

const enc = encodeURIComponent;
const p = (id: string): string => `/api/projects/${enc(id)}`;
const s = (id: string): string => `/api/sessions/${enc(id)}`;

export interface SessionQuery {
  project?: string;
  status?: SessionStatus;
  agent?: string;
  machine?: string;
  q?: string;
  cursor?: string;
  limit?: number;
  /** Children (subagent sessions) of this session. */
  parent?: string;
}

export interface SearchQuery {
  q: string;
  project?: string;
  kind?: SearchHitKind;
  limit?: number;
}

/** Projects with several folders on this machine pick one with `root` (its absolute path). */
export interface Rooted {
  root?: string | undefined;
}

export const api = {
  health: () => request<Health>('GET', '/api/health'),
  /** Asks GitHub at most once a day; updating is `blirp update` in a terminal. */
  update: () => request<UpdateStatus>('GET', '/api/update'),
  machines: {
    list: () => request<Machine[]>('GET', '/api/machines'),
    revoke: (id: string) => request<void>('DELETE', `/api/machines/${enc(id)}`),
    /** Another machine's answers are relayed through the hub. */
    health: (id: string) => request<Health>('GET', `/api/machines/${enc(id)}/health`),
    agents: (id: string) => request<AgentInfo[]>('GET', `/api/machines/${enc(id)}/agents`),
    /** Folders (never files) inside that machine's home; `path` is absolute, empty for home. */
    dirs: (id: string, path: string, hidden: boolean) =>
      request<MachineDirs>('GET', `/api/machines/${enc(id)}/dirs`, undefined, { path, hidden }),
    clone: (id: string, req: CloneRepo) => request<CloneJob>('POST', `/api/machines/${enc(id)}/clone`, req),
    cloneJob: (id: string, job: string) => request<CloneJob>('GET', `/api/machines/${enc(id)}/clone/${enc(job)}`),
  },
  projects: {
    list: () => request<ProjectSummary[]>('GET', '/api/projects'),
    get: (id: string) => request<ProjectSummary>('GET', p(id)),
    create: (path: string, name?: string) => request<ProjectSummary>('POST', '/api/projects', { path, name }),
    rename: (id: string, name: string) => request<ProjectSummary>('PATCH', p(id), { name }),
    remove: (id: string) => request<void>('DELETE', p(id)),
    merge: (id: string, into: string) => request<ProjectSummary>('POST', `${p(id)}/merge`, { into }),
    memory: (id: string) => request<ProjectMemory>('GET', `${p(id)}/memory`),
    putBrief: (id: string, body_md: string) => request<Brief>('PUT', `${p(id)}/brief`, { body_md }),
    briefHistory: (id: string) => request<Brief[]>('GET', `${p(id)}/brief/history`),
    /** Prefer the history entry's `id`: versions written on another machine can shift the numbers. */
    revertBrief: (id: string, target: RevertBrief) => request<Brief>('POST', `${p(id)}/brief/revert`, target),
    records: (id: string) => request<MemoryRecord[]>('GET', `${p(id)}/records`),
    createRecord: (id: string, r: CreateRecord) => request<MemoryRecord>('POST', `${p(id)}/records`, r),
    updateRecord: (id: string, rid: string, r: PatchRecord) =>
      request<MemoryRecord>('PATCH', `${p(id)}/records/${enc(rid)}`, r),
    deleteRecord: (id: string, rid: string) => request<void>('DELETE', `${p(id)}/records/${enc(rid)}`),
    wiki: (id: string) => request<WikiPage[]>('GET', `${p(id)}/wiki`),
    wikiPage: (id: string, slug: string) => request<WikiPage>('GET', `${p(id)}/wiki/${enc(slug)}`),
    createWiki: (id: string, w: CreateWikiPage) => request<WikiPage>('POST', `${p(id)}/wiki`, w),
    updateWiki: (id: string, slug: string, w: PutWikiPage) =>
      request<WikiPage>('PUT', `${p(id)}/wiki/${enc(slug)}`, w),
    deleteWiki: (id: string, slug: string) => request<void>('DELETE', `${p(id)}/wiki/${enc(slug)}`),
    resources: (id: string) => request<Resource[]>('GET', `${p(id)}/resources`),
    createResource: (id: string, r: CreateResource) => request<Resource>('POST', `${p(id)}/resources`, r),
    updateResource: (id: string, rid: string, r: PatchResource) =>
      request<Resource>('PATCH', `${p(id)}/resources/${enc(rid)}`, r),
    deleteResource: (id: string, rid: string) => request<void>('DELETE', `${p(id)}/resources/${enc(rid)}`),
    suggestions: (id: string, status?: SuggestionStatus) =>
      request<Suggestion[]>('GET', `${p(id)}/suggestions`, undefined, { status }),
    git: (id: string, q: Rooted = {}) => request<GitStatus>('GET', `${p(id)}/git`, undefined, { root: q.root }),
    gitDiff: (id: string, path: string, q: Rooted = {}) =>
      request<GitDiff>('GET', `${p(id)}/git/diff`, undefined, { path, root: q.root }),
    files: (id: string, path: string, q: Rooted = {}) =>
      request<DirListing>('GET', `${p(id)}/files`, undefined, { path, root: q.root }),
    fileContent: (id: string, path: string, q: Rooted = {}) =>
      request<FileContent>('GET', `${p(id)}/files/content`, undefined, { path, root: q.root }),
  },
  suggestions: {
    decide: (id: string, decision: 'accept' | 'reject' | 'dismiss') =>
      request<Suggestion>('POST', `/api/suggestions/${enc(id)}/${decision}`),
  },
  sessions: {
    list: (q: SessionQuery = {}) => request<SessionsPage>('GET', '/api/sessions', undefined, { ...q }),
    launch: (body: LaunchSession) => request<Session>('POST', '/api/sessions', body),
    get: (id: string) => request<SessionDetail>('GET', s(id)),
    events: (id: string, after: number, limit: number) =>
      request<EventsPage>('GET', `${s(id)}/events`, undefined, { after, limit }),
    /** 202: the kill is under way; the final status arrives as a `session_updated` event. */
    stop: (id: string) => request<void>('POST', `${s(id)}/stop`),
    resume: (id: string) => request<Session>('POST', `${s(id)}/resume`),
    distill: (id: string) => request<void>('POST', `${s(id)}/distill`),
    rename: (id: string, title: string | null) => request<Session>('PATCH', s(id), { title }),
    open: (id: string, target: OpenTarget) => request<void>('POST', `${s(id)}/open`, { target }),
    /** Ended sessions only (409 `session_live`); other clients hear `session_deleted`. */
    delete: (id: string) => request<void>('DELETE', s(id)),
    /** 409 `worktree_dirty` unless `force`; the `blirp/<name>` branch is kept. */
    removeWorktree: (id: string, force = false) => request<Session>('POST', `${s(id)}/worktree/remove`, { force }),
  },
  search: (q: SearchQuery) => request<SearchResults>('GET', '/api/search', undefined, { ...q }),
  agents: {
    list: () => request<AgentInfo[]>('GET', '/api/agents'),
    installHooks: (id: string) => request<AgentInfo>('POST', `/api/agents/${enc(id)}/hooks/install`),
    uninstallHooks: (id: string) => request<AgentInfo>('POST', `/api/agents/${enc(id)}/hooks/uninstall`),
  },
  inject: (session: string) => request<Injection>('GET', '/api/inject', undefined, { session }),
  settings: {
    get: () => request<SettingsView>('GET', '/api/settings'),
    /** `config` replaces the whole config file; `values` keys are set (null deletes). */
    patch: (patch: SettingsPatch) => request<SettingsView>('PATCH', '/api/settings', patch),
  },
  sync: {
    status: () => request<SyncStatus>('GET', '/api/sync/status'),
    enableHub: () => request<SyncStatus>('POST', '/api/sync/hub/enable'),
    disableHub: () => request<SyncStatus>('POST', '/api/sync/hub/disable'),
    invite: () => request<SyncInvite>('POST', '/api/sync/invite'),
    /** `invite` may be a `blirp://join` URI, or empty to find the hub on the local network. */
    join: (body: JoinHub) => request<SyncStatus>('POST', '/api/sync/join', body),
    /** The hub an invite points at, read from the invite alone (nothing is contacted). */
    previewJoin: (invite: string) => request<JoinPreview>('POST', '/api/sync/join/preview', { invite }),
  },
  devices: {
    list: () => request<Device[]>('GET', '/api/devices'),
    revoke: (id: string) => request<void>('DELETE', `/api/devices/${enc(id)}`),
    patch: (id: string, body: PatchDevice) => request<Device>('PATCH', `/api/devices/${enc(id)}`, body),
    browserInvite: () => request<BrowserInvite>('POST', '/api/devices/browser-invite'),
  },
  /** 202; loopback listener only (404 on the portal). */
  shutdown: () => request<void>('POST', '/api/daemon/shutdown'),
};

/** Absolute ws:// or wss:// URL for a same-origin path. */
export function wsUrl(path: string, loc: Pick<Location, 'protocol' | 'host'> = location): string {
  return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}${path}`;
}

/**
 * URL to open a same-origin WebSocket with. A WebSocket cannot carry `Authorization`, so a
 * signed-in local page first trades its token for a single-use ticket bound to this path (valid
 * 30 s); portal pages send their device cookie with the upgrade instead.
 */
export async function socketUrl(path: string, loc: Pick<Location, 'protocol' | 'host'> = location): Promise<string> {
  const url = wsUrl(path, loc);
  if (authToken() === null) return url;
  const { ticket } = await request<WsTicket>('POST', '/api/ws-ticket', { path });
  return `${url}?ticket=${enc(ticket)}`;
}

export const terminalWsPath = (id: string): string => `/api/terminals/${enc(id)}/ws`;
export const eventsWsPath = '/api/events/ws';
