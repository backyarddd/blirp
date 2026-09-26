<script lang="ts">
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import type { Session } from '../api/types';
  import { href } from '../router';
  import { agentLabel, basename, sessionTitle } from '../status';
  import { formatRelative } from '../time';
  import StatusChip from './StatusChip.svelte';

  let { session }: { session: Session } = $props();
</script>

<a class="srow" href={href.sessions(session.id)}>
  <span class="main">
    <span class="title ellipsis">{sessionTitle(session)}</span>
    <span class="meta small muted">
      <span>{agentLabel(session.agent)}</span>
      <span class="where">
        {#if session.branch}<GitBranch size={12} aria-label="Branch" />{session.branch}{:else}<Folder size={12} aria-label="Folder" />{basename(session.cwd)}{/if}
      </span>
      <span>{formatRelative(session.started_at)}</span>
      {#if session.origin === 'external'}<span class="badge">external</span>{/if}
    </span>
  </span>
  <StatusChip status={session.status} />
</a>

<style>
  .srow {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 4px;
    text-decoration: none;
    border-radius: 8px;
  }
  .srow:hover {
    background: var(--panel-2);
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
