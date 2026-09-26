// Shapes the UI already relies on for endpoints the daemon does not serve yet (they answer
// 501 `not_implemented`, ARCHITECTURE §11). Everything the daemon serves today comes from
// `types.gen.ts`. When a phase lands, delete its entries here and import the generated type.
//
// - sync phase (`blirp-sync`): SyncStatus (`GET /api/sync/status`, also returned by
//   `POST /api/sync/hub/enable` and `POST /api/sync/join`), Invite (`POST /api/sync/invite`),
//   JoinRequest, BrowserInvite (`POST /api/devices/browser-invite`), DevicePatch
//   (`PATCH /api/devices/:id`). Devices themselves use the generated `Device`.
// - memory phase (hooks, inject, distill): Injection (`GET /api/inject`), AgentIntegration
//   (`AgentInfo.integration`, `POST /api/agents/:id/hooks/install|uninstall`) and
//   SessionSummary (distill output §9, delivered as the untyped `Session.summary` JSON).

import type { AgentInfo, MachineRole } from './types.gen';

export interface SyncStatus {
  role: MachineRole;
  machine_id: string;
  hub: string | null;
  connected: boolean;
  last_sync_at: number | null;
  pending_outbox: number;
  portal_url: string | null;
  portal_cert_fingerprint: string | null;
}

export interface Invite {
  invite: string;
  code: string;
  /** `blirp://join/<ticket>#<code>` */
  uri: string;
  expires_at: number;
}

export interface JoinRequest {
  invite: string;
  code: string;
}

export interface BrowserInvite {
  url: string;
  expires_at: number;
}

export interface DevicePatch {
  can_control_terminals: boolean;
}

export interface Injection {
  markdown: string;
}

export interface AgentIntegration {
  /** Global hooks installed in the agent's own user config. */
  global_hooks: boolean;
  /** blirp MCP server registered globally. */
  mcp: boolean;
  /** Memory is injected at session start for sessions started outside blirp. */
  inject: boolean;
}

/** `AgentInfo` as the hooks phase will serve it; `integration` is absent until then. */
export type AgentView = AgentInfo & { integration?: AgentIntegration };

export interface TitledNote {
  title: string;
  body: string;
}

/** Distill output (§9) stored on a session. */
export interface SessionSummary {
  title: string;
  summary: string;
  decisions: TitledNote[];
  open_threads: TitledNote[];
  resolved_record_ids: string[];
  gotchas: TitledNote[];
  files: string[];
  brief_md: string;
}
