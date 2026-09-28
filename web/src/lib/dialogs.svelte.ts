// State of the app-wide action dialogs (ActionDialogs.svelte): confirmations and the forms behind
// menu items that need input (rename, move, merge, add folder, continue in). One of each at a time.
import type { ProjectSummary, Record as MemoryRecord, Session } from './api/types.gen';

export interface ConfirmOptions {
  title: string;
  /** What happens, in a sentence or two. */
  body: string;
  /** Items the action applies to, listed under the body (bulk actions). */
  list?: string[];
  /** Label of the confirming button, e.g. "Delete". */
  confirm: string;
  danger?: boolean;
}

interface PendingConfirm extends ConfirmOptions {
  resolve: (ok: boolean) => void;
}

export type RenameTarget = { kind: 'session'; session: Session } | { kind: 'project'; project: ProjectSummary };

class Dialogs {
  confirming: PendingConfirm | null = $state(null);
  renaming: RenameTarget | null = $state(null);
  /** Sessions to move (one, or a bulk selection). */
  moving: Session[] | null = $state(null);
  /** Memory records to move to another project (one, or a bulk selection). */
  movingRecords: MemoryRecord[] | null = $state(null);
  merging: ProjectSummary | null = $state(null);
  addingFolder: ProjectSummary | null = $state(null);
  continuing: Session | null = $state(null);
  /** A worktree removal git refused: the session and git's message, for the force prompt. */
  dirtyWorktree: { session: Session; message: string } | null = $state(null);

  /** Ask before a destructive action; resolves false on Cancel, Esc or a click outside. */
  confirm(options: ConfirmOptions): Promise<boolean> {
    this.confirming?.resolve(false);
    return new Promise((resolve) => {
      this.confirming = { ...options, resolve };
    });
  }

  settleConfirm(ok: boolean): void {
    const c = this.confirming;
    this.confirming = null;
    c?.resolve(ok);
  }
}

export const dialogs = new Dialogs();
