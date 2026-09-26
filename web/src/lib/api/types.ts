// Provisional DTOs modeled on ARCHITECTURE.md §5 and §11. The daemon will generate the
// authoritative types into `types.gen.ts` (ts-rs); when it lands, re-export from there and
// delete what matches. Conventions: ids are strings, timestamps are unix milliseconds.

export type Id = string;
export type UnixMs = number;

export type SyncRole = 'standalone' | 'node' | 'hub';
export type SessionStatus =
  | 'starting'
  | 'working'
  | 'idle'
  | 'waiting'
  | 'completed'
  | 'failed'
  | 'detached';
export type SessionOrigin = 'blirp' | 'external';
export type EventKind =
  | 'user'
  | 'assistant'
  | 'tool_call'
  | 'tool_result'
  | 'system'
  | 'file_edit'
  | 'summary';
export type RecordKind = 'decision' | 'plan' | 'note' | 'open_thread' | 'gotcha';
export type RecordStatus = 'active' | 'resolved' | 'archived';
export type SuggestionTarget = 'brief' | 'record' | 'wiki';
export type SuggestionStatus = 'pending' | 'accepted' | 'rejected' | 'dismissed';
export type ResourceKind = 'link' | 'repo' | 'pr' | 'issue' | 'doc' | 'file';
export type DeviceKind = 'machine' | 'browser';
export type Summarizer = 'auto' | 'claude' | 'codex' | 'ollama' | 'none';
export type BriefMode = 'auto' | 'review';

export interface ApiErrorBody {
  error: { code: string; message: string };
}

export interface Page<T> {
  items: T[];
  next_cursor: string | null;
}

export interface Health {
  version: string;
  machine: Id;
  role: SyncRole;
}

export interface Machine {
  id: Id;
  name: string;
  os: string;
  role: SyncRole;
  last_seen: UnixMs | null;
  revoked: boolean;
}

export interface ProjectPath {
  machine_id: Id;
  path: string;
  git_remote: string | null;
  is_git: boolean;
}

export interface Project {
  id: Id;
  name: string;
  created_at: UnixMs;
  updated_at: UnixMs;
  paths: ProjectPath[];
  is_git: boolean;
  session_count: number;
  live_session_count: number;
  last_activity_at: UnixMs | null;
  machine_ids: Id[];
}

export interface Session {
  id: Id;
  project_id: Id;
  machine_id: Id;
  /** claude|codex|opencode|pi|gemini|cursor|amp|aider|dsh|shell|custom:<name> */
  agent: string;
  agent_session_id: string | null;
  origin: SessionOrigin;
  cwd: string;
  title: string | null;
  status: SessionStatus;
  branch: string | null;
  worktree: string | null;
  started_at: UnixMs;
  ended_at: UnixMs | null;
  last_activity_at: UnixMs;
  exit_code: number | null;
  tokens_in: number;
  tokens_out: number;
  cost_usd: number;
  parent_session_id: Id | null;
}

export interface TitledNote {
  title: string;
  body: string;
}

/** Distill output, §8. */
export interface SessionSummary {
  title: string;
  summary: string;
  decisions: TitledNote[];
  open_threads: TitledNote[];
  resolved_record_ids: Id[];
  gotchas: TitledNote[];
  files: string[];
  brief_md: string;
}

export interface SessionDetail extends Session {
  summary: SessionSummary | null;
}

export interface SessionEvent {
  session_id: Id;
  seq: number;
  ts: UnixMs;
  kind: EventKind;
  text: string;
  meta: Record<string, unknown> | null;
}

export interface LaunchSessionRequest {
  project_id?: Id;
  cwd?: string;
  agent: string;
  prompt?: string;
  worktree?: boolean;
  continue_from?: Id;
  machine?: Id;
}

export interface SessionQuery {
  project?: Id;
  status?: SessionStatus;
  agent?: string;
  machine?: Id;
  q?: string;
  cursor?: string;
  limit?: number;
}

export interface MemoryRecord {
  id: Id;
  project_id: Id;
  kind: RecordKind;
  title: string;
  body: string;
  status: RecordStatus;
  pinned: boolean;
  source_session_id: Id | null;
  created_at: UnixMs;
  updated_at: UnixMs;
  updated_by: string;
}

export interface RecordInput {
  kind: RecordKind;
  title: string;
  body: string;
  status?: RecordStatus;
  pinned?: boolean;
}

export interface Brief {
  project_id: Id;
  body_md: string;
  version: number;
  updated_at: UnixMs;
  updated_by: string;
}

export interface BriefVersion {
  version: number;
  body_md: string;
  updated_at: UnixMs;
  updated_by: string;
}

export interface ProjectMemory {
  brief: Brief | null;
  records: MemoryRecord[];
  recent_sessions: Session[];
}

export interface WikiPage {
  id: Id;
  project_id: Id;
  slug: string;
  title: string;
  body_md: string;
  updated_at: UnixMs;
  updated_by: string;
}

export interface WikiPageInput {
  slug: string;
  title: string;
  body_md: string;
}

export interface Suggestion {
  id: Id;
  project_id: Id;
  target: SuggestionTarget;
  target_id: Id | null;
  /** Parsed `proposal_json`: brief -> {body_md}; record -> RecordInput; wiki -> WikiPageInput. */
  proposal: Record<string, unknown>;
  rationale: string;
  source_session_id: Id | null;
  status: SuggestionStatus;
  created_at: UnixMs;
  decided_at: UnixMs | null;
}

export interface Resource {
  id: Id;
  project_id: Id;
  kind: ResourceKind;
  url: string;
  title: string;
  meta: Record<string, unknown> | null;
  created_at: UnixMs;
}

export interface ResourceInput {
  kind: ResourceKind;
  url: string;
  title: string;
}

export interface GitFileStatus {
  path: string;
  /** Porcelain XY code, e.g. "M ", "??", "A ". */
  status: string;
}

export interface GitStatus {
  is_git: boolean;
  branch: string | null;
  ahead: number;
  behind: number;
  status: GitFileStatus[];
}

export interface GitDiff {
  path: string;
  diff: string;
}

export interface FileEntry {
  name: string;
  /** Relative to the project root, forward slashes. */
  path: string;
  kind: 'file' | 'dir';
  size: number | null;
}

export interface FileListing {
  path: string;
  entries: FileEntry[];
}

export interface FileContent {
  path: string;
  content: string;
  binary: boolean;
  truncated: boolean;
}

export type SearchKind = 'event' | 'record';

export interface SearchHit {
  kind: SearchKind;
  /** Event: `<session_id>:<seq>`; record: record id. */
  id: string;
  project_id: Id;
  project_name: string;
  session_id: Id | null;
  seq: number | null;
  agent: string | null;
  record_kind: RecordKind | null;
  title: string;
  /** FTS5 snippet, matches wrapped in U+0002 ... U+0003. */
  snippet: string;
  ts: UnixMs;
}

export interface SearchQuery {
  q: string;
  project?: Id;
  kind?: SearchKind;
}

export interface AgentIntegration {
  supported: boolean;
  installed: boolean;
  detail: string | null;
}

export interface AgentInfo {
  id: string;
  name: string;
  installed: boolean;
  version: string | null;
  path: string | null;
  custom: boolean;
  integration: AgentIntegration;
}

export interface Injection {
  markdown: string;
}

export interface Settings {
  daemon: { port: number };
  machine: { name: string };
  agents: { default: string };
  sessions: { worktree_default: boolean };
  memory: {
    summarizer: Summarizer;
    ollama_model: string;
    distill_idle_secs: number;
    daily_distill_limit: number;
    brief_mode: BriefMode;
    inject_max_chars: number;
    distill_max_chars: number;
  };
  sync: { role: SyncRole; hub: string | null; relay: string };
  portal: { lan: boolean; lan_port: number };
}

export type SettingsPatch = {
  [K in keyof Settings]?: Partial<Settings[K]>;
};

export interface SyncStatus {
  role: SyncRole;
  machine_id: Id;
  hub: Id | null;
  connected: boolean;
  last_sync_at: UnixMs | null;
  pending_outbox: number;
  portal_url: string | null;
  portal_cert_fingerprint: string | null;
}

export interface Invite {
  invite: string;
  code: string;
  /** `blirp://join/<ticket>#<code>` */
  uri: string;
  expires_at: UnixMs;
}

export interface BrowserInvite {
  url: string;
  expires_at: UnixMs;
}

export interface Device {
  id: Id;
  name: string;
  kind: DeviceKind;
  node_id: string | null;
  created_at: UnixMs;
  last_seen: UnixMs | null;
  revoked: boolean;
  can_control_terminals: boolean;
}

/** Messages pushed on `/api/events/ws`. */
export type ServerEvent =
  | { type: 'session'; session: Session }
  | { type: 'session_removed'; id: Id }
  | { type: 'project'; project: Project }
  | { type: 'memory'; project_id: Id };
