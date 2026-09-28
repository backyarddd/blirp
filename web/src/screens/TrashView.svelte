<script lang="ts">
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import { api, errorMessage } from '../lib/api/client';
  import type { ProjectSummary } from '../lib/api/types.gen';
  import { app } from '../lib/app.svelte';
  import { href } from '../lib/router';
  import { formatRelative } from '../lib/time';
  import { trashActions } from '../lib/actions';
  import { actionEnv, projectOps, restoreProject } from '../lib/manage';
  import { contextmenu } from '../lib/contextmenu';
  import Loadable from '../lib/components/Loadable.svelte';
  import Menu from '../lib/components/Menu.svelte';

  let items: ProjectSummary[] = $state.raw([]);
  let loaded = $state(false);
  let error: string | null = $state(null);
  let busy = $state('');

  async function load(): Promise<void> {
    try {
      items = await api.projects.trash();
      error = null;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      loaded = true;
    }
  }

  // A delete or restore anywhere (this client or another) changes the live projects: read again.
  $effect(() => {
    void app.projects;
    void load();
  });

  async function restore(p: ProjectSummary): Promise<void> {
    busy = p.id;
    const out = await restoreProject(p);
    busy = '';
    if (out) items = items.filter((x) => x.id !== p.id);
  }
</script>

<div class="page">
  <div class="page-inner">
    <nav class="crumbs small" aria-label="Breadcrumb"><a href={href.projects()}>Projects</a> / <span aria-current="page">Trash</span></nav>
    <h1 class="page-title">Trash</h1>
    <p class="muted sub">
      Deleted projects wait here with their sessions and memory; their sessions are hidden until you restore them. Restoring
      brings a project back with its sessions and the folders it had on this machine. Folders on other machines are not
      restored: add them again there. Files on disk were never touched.
    </p>
    <Loadable loading={!loaded} {error} empty={items.length === 0} emptyText="The Trash is empty." onretry={load}>
      <ul class="rows card">
        {#each items as p (p.id)}
          <li class="row" use:contextmenu={{ items: () => trashActions(p, actionEnv(), projectOps), label: p.name }}>
            <div class="main">
              <strong class="ellipsis">{p.name}</strong>
              <span class="small muted">
                {p.session_count}
                {p.session_count === 1 ? 'session' : 'sessions'} · deleted {formatRelative(p.updated_at)}
              </span>
            </div>
            {#if app.admin}
              <button type="button" class="btn sm" disabled={busy === p.id} onclick={() => restore(p)}
                ><RotateCcw size={14} aria-hidden="true" />Restore</button
              >
            {/if}
            <Menu items={trashActions(p, actionEnv(), projectOps)} label="Actions for {p.name}" title="More actions" triggerClass="icon-btn sm" align="right"
              ><Ellipsis size={15} /></Menu
            >
          </li>
        {/each}
      </ul>
      {#if !app.admin}<p class="small muted">Only this machine's own app or CLI can restore projects.</p>{/if}
    </Loadable>
  </div>
</div>

<style>
  .crumbs {
    color: var(--text-3);
    margin-bottom: 6px;
  }
  .crumbs a {
    color: var(--text-2);
    text-decoration: none;
  }
  .sub {
    margin: 0 0 16px;
    max-width: 720px;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 4px 12px;
  }
  .rows li {
    gap: 10px;
    padding: 10px 0;
    border-bottom: 1px solid var(--border);
  }
  .rows li:last-child {
    border-bottom: 0;
  }
  .main {
    display: grid;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
</style>
