// The only module that talks HTTP to the daemon. Everything else imports `api`.
import type {
  AgentInfo,
  Brief,
  BriefVersion,
  BrowserInvite,
  Device,
  FileContent,
  FileListing,
  GitDiff,
  GitStatus,
  Health,
  Id,
  Injection,
  Invite,
  LaunchSessionRequest,
  Machine,
  MemoryRecord,
  Page,
  Project,
  ProjectMemory,
  RecordInput,
  Resource,
  ResourceInput,
  SearchHit,
  SearchQuery,
  Session,
  SessionDetail,
  SessionEvent,
  SessionQuery,
  Settings,
  SettingsPatch,
  Suggestion,
  SyncStatus,
  WikiPage,
  WikiPageInput,
} from './types';

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
  const init: RequestInit = { method, credentials: 'same-origin', headers: { Accept: 'application/json' } };
  if (body !== undefined) {
    init.headers = { Accept: 'application/json', 'Content-Type': 'application/json' };
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
const p = (id: Id): string => `/api/projects/${enc(id)}`;
const s = (id: Id): string => `/api/sessions/${enc(id)}`;

export const api = {
  health: () => request<Health>('GET', '/api/health'),
  machines: {
    list: () => request<Machine[]>('GET', '/api/machines'),
    revoke: (id: Id) => request<void>('DELETE', `/api/machines/${enc(id)}`),
  },
  projects: {
    list: () => request<Project[]>('GET', '/api/projects'),
    create: (path: string, name?: string) => request<Project>('POST', '/api/projects', { path, name }),
    rename: (id: Id, name: string) => request<Project>('PATCH', p(id), { name }),
    remove: (id: Id) => request<void>('DELETE', p(id)),
    merge: (id: Id, into: Id) => request<Project>('POST', `${p(id)}/merge`, { into }),
    memory: (id: Id) => request<ProjectMemory>('GET', `${p(id)}/memory`),
    putBrief: (id: Id, body_md: string) => request<Brief>('PUT', `${p(id)}/brief`, { body_md }),
    briefHistory: (id: Id) => request<BriefVersion[]>('GET', `${p(id)}/brief/history`),
    revertBrief: (id: Id, version: number) => request<Brief>('POST', `${p(id)}/brief/revert`, { version }),
    records: (id: Id) => request<MemoryRecord[]>('GET', `${p(id)}/records`),
    createRecord: (id: Id, r: RecordInput) => request<MemoryRecord>('POST', `${p(id)}/records`, r),
    updateRecord: (id: Id, rid: Id, r: Partial<RecordInput>) =>
      request<MemoryRecord>('PATCH', `${p(id)}/records/${enc(rid)}`, r),
    deleteRecord: (id: Id, rid: Id) => request<void>('DELETE', `${p(id)}/records/${enc(rid)}`),
    wiki: (id: Id) => request<WikiPage[]>('GET', `${p(id)}/wiki`),
    wikiPage: (id: Id, slug: string) => request<WikiPage>('GET', `${p(id)}/wiki/${enc(slug)}`),
    createWiki: (id: Id, w: WikiPageInput) => request<WikiPage>('POST', `${p(id)}/wiki`, w),
    updateWiki: (id: Id, slug: string, w: WikiPageInput) =>
      request<WikiPage>('PUT', `${p(id)}/wiki/${enc(slug)}`, w),
    deleteWiki: (id: Id, slug: string) => request<void>('DELETE', `${p(id)}/wiki/${enc(slug)}`),
    resources: (id: Id) => request<Resource[]>('GET', `${p(id)}/resources`),
    createResource: (id: Id, r: ResourceInput) => request<Resource>('POST', `${p(id)}/resources`, r),
    updateResource: (id: Id, rid: Id, r: ResourceInput) =>
      request<Resource>('PATCH', `${p(id)}/resources/${enc(rid)}`, r),
    deleteResource: (id: Id, rid: Id) => request<void>('DELETE', `${p(id)}/resources/${enc(rid)}`),
    suggestions: (id: Id) => request<Suggestion[]>('GET', `${p(id)}/suggestions`),
    git: (id: Id) => request<GitStatus>('GET', `${p(id)}/git`),
    gitDiff: (id: Id, path: string) => request<GitDiff>('GET', `${p(id)}/git/diff`, undefined, { path }),
    files: (id: Id, path: string) => request<FileListing>('GET', `${p(id)}/files`, undefined, { path }),
    fileContent: (id: Id, path: string) =>
      request<FileContent>('GET', `${p(id)}/files/content`, undefined, { path }),
  },
  suggestions: {
    decide: (id: Id, decision: 'accept' | 'reject' | 'dismiss') =>
      request<Suggestion>('POST', `/api/suggestions/${enc(id)}/${decision}`),
  },
  sessions: {
    list: (q: SessionQuery = {}) => request<Page<Session>>('GET', '/api/sessions', undefined, { ...q }),
    launch: (body: LaunchSessionRequest) => request<Session>('POST', '/api/sessions', body),
    get: (id: Id) => request<SessionDetail>('GET', s(id)),
    events: (id: Id, after: number, limit: number) =>
      request<SessionEvent[]>('GET', `${s(id)}/events`, undefined, { after, limit }),
    stop: (id: Id) => request<Session>('POST', `${s(id)}/stop`),
    resume: (id: Id) => request<Session>('POST', `${s(id)}/resume`),
    distill: (id: Id) => request<void>('POST', `${s(id)}/distill`),
    rename: (id: Id, title: string) => request<Session>('PATCH', s(id), { title }),
    open: (id: Id, target: 'folder' | 'editor') => request<void>('POST', `${s(id)}/open`, { target }),
  },
  search: (q: SearchQuery) => request<SearchHit[]>('GET', '/api/search', undefined, { ...q }),
  agents: {
    list: () => request<AgentInfo[]>('GET', '/api/agents'),
    installHooks: (id: string) => request<AgentInfo>('POST', `/api/agents/${enc(id)}/hooks/install`),
    uninstallHooks: (id: string) => request<AgentInfo>('POST', `/api/agents/${enc(id)}/hooks/uninstall`),
  },
  inject: (session: Id) => request<Injection>('GET', '/api/inject', undefined, { session }),
  settings: {
    get: () => request<Settings>('GET', '/api/settings'),
    patch: (patch: SettingsPatch) => request<Settings>('PATCH', '/api/settings', patch),
  },
  sync: {
    status: () => request<SyncStatus>('GET', '/api/sync/status'),
    enableHub: () => request<SyncStatus>('POST', '/api/sync/hub/enable'),
    invite: () => request<Invite>('POST', '/api/sync/invite'),
    join: (invite: string, code: string) => request<SyncStatus>('POST', '/api/sync/join', { invite, code }),
  },
  devices: {
    list: () => request<Device[]>('GET', '/api/devices'),
    revoke: (id: Id) => request<void>('DELETE', `/api/devices/${enc(id)}`),
    setControl: (id: Id, can_control_terminals: boolean) =>
      request<Device>('PATCH', `/api/devices/${enc(id)}`, { can_control_terminals }),
    browserInvite: () => request<BrowserInvite>('POST', '/api/devices/browser-invite'),
  },
};

/** Absolute ws:// or wss:// URL for a same-origin path. */
export function wsUrl(path: string, loc: Pick<Location, 'protocol' | 'host'> = location): string {
  return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}${path}`;
}

export const terminalWsPath = (id: Id): string => `/api/terminals/${enc(id)}/ws`;
export const eventsWsPath = '/api/events/ws';
