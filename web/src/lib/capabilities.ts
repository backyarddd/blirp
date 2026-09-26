import type { Capabilities } from './api/types.gen';

/** What the UI offers this client (§11). */
export interface Rights {
  /** Settings writes, hooks install, sync admin, devices, invites, open folder/editor. */
  admin: boolean;
  /** Launch, stop, resume, delete sessions, type into terminals, change memory. */
  control: boolean;
  /** This machine's own client (runtime token): may stop the daemon. */
  local: boolean;
}

/** Rights a 403 revoked since capabilities were last read. */
export interface Denied {
  admin: boolean;
  control: boolean;
}

export const NONE_DENIED: Denied = { admin: false, control: false };

/**
 * `GET /api/health` capabilities, narrowed by any 403 seen since: a device's rights can change
 * between the health read and its socket reconnecting. Unknown capabilities grant nothing.
 */
export function rightsFrom(caps: Capabilities | null | undefined, denied: Denied): Rights {
  return {
    admin: (caps?.admin ?? false) && !denied.admin,
    control: (caps?.control_terminals ?? false) && !denied.control,
    local: (caps?.local ?? false) && !denied.admin,
  };
}

/** Folds a 403 error code (`admin_only`, `control_not_allowed`) into the denied set. */
export function denyFor(code: string, denied: Denied): Denied {
  if (code === 'admin_only') return { ...denied, admin: true };
  if (code === 'control_not_allowed') return { ...denied, control: true };
  return denied;
}
