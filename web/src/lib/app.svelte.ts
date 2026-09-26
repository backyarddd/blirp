import { ApiError, api, errorMessage, eventsWsPath, onUnauthorized, wsUrl } from './api/client';
import type { Health, LaunchSession, ProjectSummary, ServerEvent, Session } from './api/types.gen';
import type { AgentView } from './api/types.pending';
import { backoffDelay } from './terminal/protocol';
import { hasTerminal, notifiableTransition, sessionTitle, statusInfo } from './status';
import { readPref, writePref } from './prefs';
import { navigate } from './router.svelte';
import { href } from './router';

export type AuthState = 'checking' | 'ok' | 'unauthorized' | 'offline';
export type ConnState = 'connecting' | 'open' | 'reconnecting';

export interface Toast {
  id: number;
  kind: 'error' | 'info';
  text: string;
}

const SIDEBAR_LIMIT = 200;
const MEMORY_PANEL_PREF = readPref('blirp.memoryPanel', ['open', 'closed', 'unset'], 'unset');

const byStartedDesc = (a: Session, b: Session): number => b.started_at - a.started_at;

const EVENT_TYPES: ReadonlySet<string> = new Set([
  'session_created',
  'session_updated',
  'project_updated',
  'memory_updated',
  'resync',
]);

/** Shallow check of a daemon frame; payloads are generated DTOs from the same-origin daemon. */
function isServerEvent(v: unknown): v is ServerEvent {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as { type?: unknown; session?: unknown; project_id?: unknown };
  if (typeof o.type !== 'string' || !EVENT_TYPES.has(o.type)) return false;
  if (o.type === 'session_created' || o.type === 'session_updated') return typeof o.session === 'object' && o.session !== null;
  if (o.type === 'resync') return true;
  return typeof o.project_id === 'string';
}

class AppState {
  auth: AuthState = $state('checking');
  bootError: string | null = $state(null);
  health: Health | null = $state.raw(null);

  sessions: Session[] = $state.raw([]);
  sessionsLoaded = $state(false);
  sessionsError: string | null = $state(null);
  projects: ProjectSummary[] = $state.raw([]);
  projectsLoaded = $state(false);
  projectsError: string | null = $state(null);
  agents: AgentView[] = $state.raw([]);
  agentsError: string | null = $state(null);
  agentsLoaded = $state(false);

  conn: ConnState = $state('connecting');
  /** Bumped per project when the daemon reports a memory change; views re-fetch on change. */
  memoryTick: Record<string, number> = $state({});
  toasts: Toast[] = $state([]);

  paletteOpen = $state(false);
  newSession: { open: boolean; projectId: string | null } = $state({ open: false, projectId: null });
  sidebarOpen = $state(false);
  // Defaults to open only where it fits beside the terminal; an explicit choice is remembered.
  memoryPanel = $state(MEMORY_PANEL_PREF === 'unset' ? window.innerWidth > 1100 : MEMORY_PANEL_PREF === 'open');
  notify = $state(readPref('blirp.notify', ['on', 'off'], 'off') === 'on');

  projectById: Map<string, ProjectSummary> = $derived(new Map(this.projects.map((p) => [p.id, p])));
  sessionById: Map<string, Session> = $derived(new Map(this.sessions.map((s) => [s.id, s])));
  liveSessions: Session[] = $derived(this.sessions.filter(hasTerminal));

  #ws: WebSocket | null = null;
  #attempt = 0;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #stopped = true;
  #toastId = 0;

  async boot(): Promise<void> {
    onUnauthorized(() => {
      this.auth = 'unauthorized';
      this.stopStream();
    });
    this.auth = 'checking';
    this.bootError = null;
    try {
      this.health = await api.health();
    } catch (e) {
      if (e instanceof ApiError && e.unauthorized) return;
      this.auth = 'offline';
      this.bootError = errorMessage(e);
      return;
    }
    this.auth = 'ok';
    this.startStream();
    await Promise.all([this.refreshProjects(), this.refreshSessions(), this.refreshAgents()]);
  }

  /** Role/name changes (hub enable, pairing) show up in the top bar. */
  async refreshHealth(): Promise<void> {
    try {
      this.health = await api.health();
    } catch (e) {
      this.toast(`Could not refresh machine status: ${errorMessage(e)}`);
    }
  }

  async refreshSessions(): Promise<void> {
    try {
      const page = await api.sessions.list({ limit: SIDEBAR_LIMIT });
      this.sessions = [...page.items].sort(byStartedDesc);
      this.sessionsError = null;
    } catch (e) {
      this.sessionsError = errorMessage(e);
    } finally {
      this.sessionsLoaded = true;
    }
  }

  async refreshProjects(): Promise<void> {
    try {
      this.projects = await api.projects.list();
      this.projectsError = null;
    } catch (e) {
      this.projectsError = errorMessage(e);
    } finally {
      this.projectsLoaded = true;
    }
  }

  async refreshAgents(): Promise<void> {
    try {
      this.agents = await api.agents.list();
      this.agentsError = null;
    } catch (e) {
      this.agentsError = errorMessage(e);
    } finally {
      this.agentsLoaded = true;
    }
  }

  upsertSession(s: Session): void {
    const prev = this.sessionById.get(s.id);
    this.sessions = prev
      ? this.sessions.map((x) => (x.id === s.id ? s : x))
      : [s, ...this.sessions].sort(byStartedDesc);
    if (prev) this.#maybeNotify(prev, s);
  }

  /** `project_updated` only carries the id: refetch it; a 404 means it was deleted or merged away. */
  async refreshProject(id: string): Promise<void> {
    try {
      this.upsertProject(await api.projects.get(id));
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) this.removeProject(id);
      else console.warn(`blirp: could not refresh project ${id}`, e);
    }
  }

  upsertProject(p: ProjectSummary): void {
    this.projects = this.projectById.has(p.id)
      ? this.projects.map((x) => (x.id === p.id ? p : x))
      : [...this.projects, p];
  }

  removeProject(id: string): void {
    this.projects = this.projects.filter((p) => p.id !== id);
  }

  bumpMemory(projectId: string): void {
    this.memoryTick[projectId] = (this.memoryTick[projectId] ?? 0) + 1;
  }

  toast(text: string, kind: Toast['kind'] = 'error'): void {
    const id = ++this.#toastId;
    this.toasts.push({ id, kind, text });
    setTimeout(() => this.dismissToast(id), kind === 'error' ? 8000 : 4000);
  }

  dismissToast(id: number): void {
    this.toasts = this.toasts.filter((t) => t.id !== id);
  }

  /** Runs a mutation; failures surface as a toast. Resolves undefined on failure. */
  async act<T>(fn: () => Promise<T>, success?: string): Promise<T | undefined> {
    try {
      const out = await fn();
      if (success) this.toast(success, 'info');
      return out;
    } catch (e) {
      this.toast(errorMessage(e));
      return undefined;
    }
  }

  async launch(req: LaunchSession): Promise<Session | undefined> {
    const s = await this.act(() => api.sessions.launch(req));
    if (s) {
      this.upsertSession(s);
      navigate(href.sessions(s.id));
    }
    return s;
  }

  openNewSession(projectId: string | null = null): void {
    this.paletteOpen = false;
    this.newSession = { open: true, projectId };
  }

  setMemoryPanel(open: boolean): void {
    this.memoryPanel = open;
    writePref('blirp.memoryPanel', open ? 'open' : 'closed');
  }

  async setNotify(on: boolean): Promise<void> {
    if (on) {
      if (!('Notification' in window)) {
        this.toast('This browser does not support notifications.');
        return;
      }
      const perm = await Notification.requestPermission();
      if (perm !== 'granted') {
        this.toast('Notifications are blocked for this site. Allow them in your browser settings.');
        return;
      }
    }
    this.notify = on;
    writePref('blirp.notify', on ? 'on' : 'off');
  }

  #maybeNotify(prev: Session, next: Session): void {
    if (!this.notify || !notifiableTransition(prev.status, next.status)) return;
    if (document.visibilityState === 'visible' && document.hasFocus()) return;
    if (!('Notification' in window) || Notification.permission !== 'granted') return;
    const project = this.projectById.get(next.project_id)?.name ?? '';
    const n = new Notification(sessionTitle(next), {
      body: `${statusInfo(next.status).label}${project ? ` · ${project}` : ''}`,
      tag: next.id,
    });
    n.onclick = () => {
      window.focus();
      navigate(href.sessions(next.id));
      n.close();
    };
  }

  startStream(): void {
    if (!this.#stopped) return;
    this.#stopped = false;
    this.#connect();
  }

  stopStream(): void {
    this.#stopped = true;
    clearTimeout(this.#timer);
    this.#ws?.close(1000);
    this.#ws = null;
  }

  #connect(): void {
    const reconnecting = this.#attempt > 0;
    this.conn = reconnecting ? 'reconnecting' : 'connecting';
    const ws = new WebSocket(wsUrl(eventsWsPath));
    this.#ws = ws;
    ws.onopen = () => {
      this.conn = 'open';
      // Catch up on anything missed while disconnected; the stream only carries deltas.
      if (reconnecting) void Promise.all([this.refreshSessions(), this.refreshProjects()]);
      this.#attempt = 0;
    };
    ws.onmessage = (ev: MessageEvent<unknown>) => {
      if (typeof ev.data === 'string') this.#handle(ev.data);
    };
    ws.onclose = () => {
      if (this.#ws !== ws || this.#stopped) return;
      this.#ws = null;
      this.conn = 'reconnecting';
      // A failed upgrade is indistinguishable from a network drop; an authenticated
      // request tells us whether the login expired (triggers the 401 screen).
      if (this.#attempt === 1) void this.refreshProjects();
      this.#timer = setTimeout(() => this.#connect(), backoffDelay(this.#attempt++));
    };
  }

  #handle(raw: string): void {
    let msg: unknown;
    try {
      msg = JSON.parse(raw);
    } catch {
      console.warn('blirp: ignoring malformed event frame');
      return;
    }
    // Unknown event types are ignored so older UIs keep working against newer daemons.
    if (!isServerEvent(msg)) return;
    switch (msg.type) {
      case 'session_created':
      case 'session_updated':
        this.upsertSession(msg.session);
        break;
      case 'project_updated':
        void this.refreshProject(msg.project_id);
        break;
      case 'memory_updated':
        this.bumpMemory(msg.project_id);
        break;
      case 'resync':
        // The daemon dropped events for this client; everything may be stale.
        void Promise.all([this.refreshSessions(), this.refreshProjects()]);
        for (const p of this.projects) this.bumpMemory(p.id);
        break;
    }
  }
}

export const app = new AppState();
