<script lang="ts">
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import Pin from '@lucide/svelte/icons/pin';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { href } from '../router';
  import { agentLabel, basename, sessionTitle } from '../status';
  import { formatRelative } from '../time';
  import { sessionActions } from '../actions';
  import { actionEnv, sessionOps } from '../manage';
  import { contextmenu } from '../contextmenu';
  import { sessionArchived, sessionKey } from '../marks';
  import StatusChip from './StatusChip.svelte';
  import MachineBadge from './MachineBadge.svelte';
  import Menu from './Menu.svelte';

  interface Props {
    session: Session;
    /** Selection mode: a checkbox before the row; Shift+click selects a range. */
    selecting?: boolean;
    selected?: boolean;
    onpick?: (shift: boolean) => void;
  }

  let { session, selecting = false, selected = false, onpick }: Props = $props();

  const pinned = $derived(app.pinned.has(sessionKey(session.id)));
  const archived = $derived(sessionArchived(session, app.archived, app.projectById));
</script>

<div class="wrap">
  {#if selecting}
    <input type="checkbox" aria-label="Select {sessionTitle(session)}" checked={selected} onclick={(e) => onpick?.(e.shiftKey)} />
  {/if}
  <a
    class="srow"
    class:archived
    href={href.sessions(session.id)}
    use:contextmenu={{
      items: () => sessionActions(session, actionEnv(), sessionOps),
      label: sessionTitle(session),
      rename: app.control ? () => sessionOps.rename(session) : undefined,
    }}
  >
    <span class="main">
      <span class="title ellipsis">{#if pinned}<Pin size={11} class="pin-mark" aria-label="Pinned" />{/if}{sessionTitle(session)}</span>
      <span class="meta small muted">
        <span>{agentLabel(session.agent)}</span>
        <span class="where">
          {#if session.branch}<GitBranch size={12} aria-label="Branch" />{session.branch}{:else}<Folder size={12} aria-label="Folder" />{basename(session.cwd)}{/if}
        </span>
        <span>{formatRelative(session.started_at)}</span>
        {#if session.origin === 'external'}<span class="badge">external</span>{/if}
        {#if archived}<span class="badge">archived</span>{/if}
        <MachineBadge machineId={session.machine_id} />
      </span>
    </span>
    <StatusChip {session} />
  </a>
  <Menu
    items={sessionActions(session, actionEnv(), sessionOps)}
    label="Actions for {sessionTitle(session)}"
    title="More actions"
    triggerClass="icon-btn sm"
    align="right"><Ellipsis size={15} /></Menu
  >
</div>

<style>
  .wrap {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .srow {
    display: flex;
    flex: 1;
    min-width: 0;
    align-items: center;
    gap: 12px;
    padding: 10px 4px;
    text-decoration: none;
    border-radius: 8px;
  }
  .srow:hover {
    background: var(--panel-2);
  }
  .srow.archived {
    opacity: 0.7;
  }
  .main {
    display: grid;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
  .title {
    font-weight: 600;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 10px;
  }
  .where {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
</style>
