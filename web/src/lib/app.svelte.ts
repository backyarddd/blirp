import { SvelteSet } from 'svelte/reactivity';
import { ApiError, api, errorMessage, eventsWsPath, onUnauthorized, socketUrl } from './api/client';
import type {
  AgentInfo,
  Health,
  LaunchSession,
  Machine,
  ProjectSummary,
  ServerEvent,
  Session,
  SettingsPatch,
  SettingsView,
  SyncStatus,
} from './api/types.gen';
import { backoffDelay } from './terminal/protocol';
import { hasTerminal, isSubagent, notifiableTransition, sessionStatusInfo, sessionTitle } from './status';
import { readPref, writePref } from './prefs';
import { NONE_DENIED, denyFor, rightsFrom, type Denied } from './capabilities';
import { nav, navigate } from './router.svelte';
import { href } from './router';
import { readOpenSessions, remoteMachine, sessionToRestore, type RemoteMachine } from './machines';

export type AuthState = 'checking' | 'ok' | 'unauthorized' | 'offline';
export type ConnState = 'connecting' | 'open' | 'reconnecting';

export interface Toast {
  id: number;
  kind: 'error' | 'info';
  text: string;
}

const SIDEBAR_LIMIT = 200;
/** How often machines are asked whether they keep themselves awake for live sessions. */
const AWAKE_POLL_MS = 30_000;
const MEMORY_PANEL_PREF = readPref('blirp.memoryPanel', ['open', 'closed', 'unset'], 'unset');

const byStartedDesc = (a: Session, b: Session): number => b.started_at - a.started_at;

const EVENT_TYPES: ReadonlySet<string> = new Set([
  'session_created',
  'session_updated',
  'session_deleted',
  'project_updated',
  'memory_updated',
  'sync_updated',
  'resync',
]);

/** Shallow check of a daemon frame; payloads are generated DTOs from the same-origin daemon. */
function isServerEvent(v: unknown): v is ServerEvent {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as { type?: unknown; session?: unknown; session_id?: unknown; project_id?: unknown; status?: unknown };
  if (typeof o.type !== 'string' || !EVENT_TYPES.has(o.type)) return false;
  if (o.type === 'session_created' || o.type === 'session_updated') return typeof o.session === 'object' && o.session !== null;
  if (o.type === 'session_deleted') return typeof o.session_id === 'string';
  if (o.type === 'sync_updated') return typeof o.status === 'object' && o.status !== null;
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
  agents: AgentInfo[] = $state.raw([]);
  agentsError: string | null = $state(null);
  agentsLoaded = $state(false);

  /** Pushed by `sync_updated`; null until loaded. */
  sync: SyncStatus | null = $state.raw(null);
  /** Paired machines (replicated); empty when this machine is standalone. */
  machines: Machine[] = $state.raw([]);
  machineById: Map<string, Machine> = $derived(new Map(this.machines.map((m) => [m.id, m])));
  /** Machines currently holding a sleep-prevention assertion for live sessions, by id. */
  awake: Record<string, boolean> = $state({});
  /** Bumped on every sync status change so views refetch machines and devices. */
  syncTick = $state(0);
  /** 403s seen since capabilities were last read (fallback for stale capabilities). */
  #denied: Denied = $state(NONE_DENIED);
  #rights = $derived(rightsFrom(this.health?.capabilities, this.#denied));
  /** Settings, hub, pairing, devices, invites, global integration, open folder/editor (§11). */
  admin = $derived(this.#rights.admin);
  /** Launch, stop, resume, delete sessions, type into terminals, change memory (§11). */
  control = $derived(this.#rights.control);
  /** This machine's own client: may stop the daemon. */
  local = $derived(this.#rights.local);

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

  /** Deleted this run: views holding their own fetched session lists filter these out. */
  readonly deletedSessions = new SvelteSet<string>();

  projectById: Map<string, ProjectSummary> = $derived(new Map(this.projects.map((p) => [p.id, p])));
  sessionById: Map<string, Session> = $derived(new Map(this.sessions.map((s) => [s.id, s])));
  liveSessions: Session[] = $derived(this.sessions.filter(hasTerminal));
  /** What lists show: subagent children sit under their parent's card instead. */
  topSessions: Session[] = $derived(this.sessions.filter((s) => !isSubagent(s)));

  #ws: WebSocket | null = null;
  #attempt = 0;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #stopped = true;
  /** Bumped by stopStream so a connect still waiting for its ticket gives up. */
  #gen = 0;
  #toastId = 0;
  #awakeTimer: ReturnType<typeof setInterval> | undefined;
  #awakeSoonTimer: ReturnType<typeof setTimeout> | undefined;

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
    await Promise.all([this.refreshProjects(), this.refreshSessions(), this.refreshAgents(), this.refreshSync()]);
    this.#restoreOpenSession();
    void this.refreshAwake();
    clearInterval(this.#awakeTimer);
    this.#awakeTimer = setInterval(() => void this.refreshAwake(), AWAKE_POLL_MS);
  }

  /** Opening the app without a session in the URL shows the one this client had open last. */
  #restoreOpenSession(): void {
    if (nav.route.name !== 'sessions' || nav.route.sessionId !== null) return;
    const id = sessionToRestore(readOpenSessions(), this.sessionById);
    if (id) navigate(href.sessions(id), { replace: true });
  }

  get selfId(): string | null {
    return this.health?.machine.id ?? null;
  }

  /** The hub's id when this machine is a node paired with it (cloud sessions run there). */
  get cloudId(): string | null {
    const hub = this.sync?.role === 'node' ? this.sync.hub : null;
    return hub && hub !== this.selfId ? hub : null;
  }

  /** Badge data for the machine a session runs on; null for this machine. */
  remote(machineId: string): RemoteMachine | null {
    return remoteMachine(machineId, this.selfId, this.machineById, this.sync?.hub ?? null);
  }

  machineName(id: string): string {
    return id === this.selfId ? (this.health?.machine.name ?? 'this machine') : (this.remote(id)?.name ?? id);
  }

  async refreshMachines(): Promise<void> {
    if ((this.sync?.role ?? this.health?.role ?? 'standalone') === 'standalone') {
      this.machines = [];
      return;
    }
    try {
      this.machines = (await api.machines.list()).filter((m) => !m.revoked);
    } catch (e) {
      console.warn('blirp: machines unavailable', e);
    }
  }

  #awakeSoon(): void {
    clearTimeout(this.#awakeSoonTimer);
    this.#awakeSoonTimer = setTimeout(() => void this.refreshAwake(), 1500);
  }

  /** Keep-awake state of this machine and, on a node, of the hub (relayed; it may be offline). */
  async refreshAwake(): Promise<void> {
    if (this.auth !== 'ok') return;
    const ids = [this.selfId, this.cloudId].filter((x): x is string => x !== null);
    const next: Record<string, boolean> = {};
    await Promise.all(
      ids.map(async (id) => {
        try {
          const h = id === this.selfId ? await api.health() : await api.machines.health(id);
          next[id] = h.keep_awake;
        } catch {
          next[id] = false; // offline or unreachable: nothing to show
        }
      }),
    );
    this.awake = next;
  }

  async refreshSync(): Promise<void> {
    try {
      this.setSync(await api.sync.status());
    } catch (e) {
      console.warn('blirp: sync status unavailable', e);
    }
  }

  setSync(status: SyncStatus): void {
    this.sync = status;
    this.syncTick++;
    void this.refreshMachines();
    // Health carries the role shown in the top bar and used by the machine picker.
    if (this.health && this.health.role !== status.role) void this.refreshHealth();
  }

  /** Role/name changes (hub enable, pairing) and this client's capabilities. */
  async refreshHealth(): Promise<void> {
    try {
      this.health = await api.health();
      this.#denied = NONE_DENIED;
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
    // A session started or ended somewhere: keep-awake follows within a status tick.
    if (!prev || hasTerminal(prev) !== hasTerminal(s)) this.#awakeSoon();
    this.sessions = prev
      ? this.sessions.map((x) => (x.id === s.id ? s : x))
      : [s, ...this.sessions].sort(byStartedDesc);
    if (prev) this.#maybeNotify(prev, s);
  }

  /** A deleted session (here or on another client) leaves every list, with its subagents. */
  removeSession(id: string): void {
    const gone = this.sessionById.get(id);
    this.deletedSessions.add(id);
    this.sessions = this.sessions.filter((s) => s.id !== id && !(isSubagent(s) && s.parent_session_id === id));
    if (nav.route.name === 'sessions' && nav.route.sessionId === id) navigate(href.sessions(), { replace: true });
    // Session counts are part of the project summary; the daemon only reports the delete.
    if (gone) void this.refreshProject(gone.project_id);
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
      this.noteForbidden(e);
      this.toast(errorMessage(e));
      return undefined;
    }
  }

  /**
   * Fallback for stale capabilities: a 403 hides those actions until capabilities are read
   * again, which happens right away so a changed device right shows up everywhere.
   */
  noteForbidden(e: unknown): void {
    if (!(e instanceof ApiError) || e.status !== 403) return;
    this.#denied = denyFor(e.code, this.#denied);
    void this.refreshHealth();
  }

  /**
   * `PATCH /api/settings` (admin). Every write re-applies the LAN portal config, and a portal
   * that cannot start answers 409 `portal_failed` with the config already saved (§13): the
   * saved settings are reloaded and the reason shown.
   */
  async saveSettings(patch: SettingsPatch, success: string): Promise<SettingsView | undefined> {
    try {
      const s = await api.settings.patch(patch);
      this.toast(success, 'info');
      return s;
    } catch (e) {
      if (!(e instanceof ApiError && e.code === 'portal_failed')) {
        this.noteForbidden(e);
        this.toast(errorMessage(e));
        return undefined;
      }
      this.toast(`Settings saved, but ${e.message}. Pick another port under Settings > Machines & Sync > LAN portal.`);
      void this.refreshSync();
      try {
        return await api.settings.get();
      } catch (err) {
        this.toast(`Could not reload settings: ${errorMessage(err)}`);
        return undefined;
      }
    }
  }

  /** Manual distill (§9). The 409s are expected outcomes and read as information. */
  async distill(session: Session): Promise<void> {
    try {
      await api.sessions.distill(session.id);
      this.toast('Distill queued. The summary and memory update when it finishes.', 'info');
    } catch (e) {
      this.noteForbidden(e);
      if (e instanceof ApiError && e.code === 'remote_session') {
        this.toast('This session ran on another machine. It is distilled there and its summary arrives by sync.', 'info');
      } else if (e instanceof ApiError && e.code === 'nothing_to_distill') {
        this.toast('Nothing to distill yet: this session has no transcript events.', 'info');
      } else {
        this.toast(`Distill failed: ${errorMessage(e)}`);
      }
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
    if (!this.control) {
      this.toast('This device may not start sessions. Allow terminal control for it under Settings > Machines & Sync on the hub.', 'info');
      return;
    }
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
      body: `${sessionStatusInfo(next).label}${project ? ` · ${project}` : ''}`,
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
    clearInterval(this.#awakeTimer);
    this.#stopped = true;
    this.#gen++;
    clearTimeout(this.#timer);
    this.#ws?.close(1000);
    this.#ws = null;
  }

  #connect(): void {
    const reconnecting = this.#attempt > 0;
    this.conn = reconnecting ? 'reconnecting' : 'connecting';
    const gen = this.#gen;
    const current = (): boolean => gen === this.#gen && !this.#stopped;
    // A failed ticket request retries like a dropped socket; a 401 already stopped the stream.
    socketUrl(eventsWsPath).then(
      (url) => {
        if (current()) this.#open(url, reconnecting);
      },
      () => {
        if (current()) this.#retry();
      },
    );
  }

  #retry(): void {
    this.conn = 'reconnecting';
    // A failed upgrade is indistinguishable from a network drop; an authenticated
    // request tells us whether the login expired (triggers the 401 screen).
    if (this.#attempt === 1) void this.refreshProjects();
    this.#timer = setTimeout(() => this.#connect(), backoffDelay(this.#attempt++));
  }

  #open(url: string, reconnecting: boolean): void {
    const ws = new WebSocket(url);
    this.#ws = ws;
    ws.onopen = () => {
      this.conn = 'open';
      // Catch up on anything missed while disconnected; the stream only carries deltas.
      // Changing a device's rights closes its sockets (§11), so a reconnect re-reads capabilities.
      if (reconnecting) {
        void Promise.all([this.refreshSessions(), this.refreshProjects(), this.refreshSync(), this.refreshHealth()]);
      }
      this.#attempt = 0;
    };
    ws.onmessage = (ev: MessageEvent<unknown>) => {
      if (typeof ev.data === 'string') this.#handle(ev.data);
    };
    ws.onclose = () => {
      if (this.#ws !== ws || this.#stopped) return;
      this.#ws = null;
      this.#retry();
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
      case 'session_deleted':
        this.removeSession(msg.session_id);
        break;
      case 'project_updated':
        void this.refreshProject(msg.project_id);
        break;
      case 'memory_updated':
        this.bumpMemory(msg.project_id);
        break;
      case 'sync_updated':
        this.setSync(msg.status);
        break;
      case 'resync':
        // The daemon dropped events for this client; everything may be stale.
        void Promise.all([this.refreshSessions(), this.refreshProjects(), this.refreshSync(), this.refreshHealth()]);
        for (const p of this.projects) this.bumpMemory(p.id);
        break;
    }
  }
}

export const app = new AppState();
