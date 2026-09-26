<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import { app } from '../app.svelte';
  import { href } from '../router';
  import { agentLabel, basename, groupSessions, sessionTitle } from '../status';
  import { formatRelative } from '../time';
  import StatusChip from './StatusChip.svelte';
  import Loadable from './Loadable.svelte';
  import Subagents from './Subagents.svelte';

  /** `selectedChildren`: subagent count of the selected session (lists leave subagents out). */
  let { selectedId, selectedChildren }: { selectedId: string | null; selectedChildren: number } = $props();

  let filter = $state('');

  const groups = $derived.by(() => {
    const q = filter.trim().toLowerCase();
    const list = q
      ? app.topSessions.filter((s) =>
          `${sessionTitle(s)} ${s.branch ?? ''} ${s.cwd} ${s.agent}`.toLowerCase().includes(q),
        )
      : app.topSessions;
    return groupSessions(list, app.projectById);
  });
</script>

<div class="sidebar-inner">
  <div class="search">
    <input class="input" type="search" placeholder="Filter sessions" aria-label="Filter sessions" bind:value={filter} />
  </div>
  <div class="scroll">
    <Loadable
      loading={!app.sessionsLoaded}
      error={app.sessionsError}
      empty={groups.length === 0}
      emptyText={filter ? 'No sessions match.' : 'No sessions yet. Start one and it shows up here, along with sessions you run outside blirp.'}
      onretry={() => app.refreshSessions()}
    >
      {#snippet emptyAction()}
        {#if !filter && app.control}<button class="btn primary sm" type="button" onclick={() => app.openNewSession()}>New session</button>{/if}
      {/snippet}
      {#each groups as g (g.projectId)}
        <section class="group" aria-label={g.name}>
          <header>
            <a class="gname ellipsis" href={href.project(g.projectId)}>{g.name}</a>
            {#if app.control}
              <button
                type="button"
                class="icon-btn sm"
                aria-label="New session in {g.name}"
                title="New session in {g.name}"
                onclick={() => app.openNewSession(g.projectId)}><Plus size={14} /></button
              >
            {/if}
          </header>
          <ul class="list-plain">
            {#each g.sessions as s (s.id)}
              <li>
                <a
                  class="scard"
                  class:selected={s.id === selectedId}
                  href={href.sessions(s.id)}
                  aria-current={s.id === selectedId ? 'page' : undefined}
                  onclick={() => (app.sidebarOpen = false)}
                >
                  <span class="title ellipsis">{sessionTitle(s)}</span>
                  <span class="meta">
                    <span class="where ellipsis">
                      {#if s.branch}
                        <GitBranch size={12} aria-label="Branch" />{s.branch}
                      {:else}
                        <Folder size={12} aria-label="Folder" />{basename(s.cwd)}
                      {/if}
                    </span>
                    <StatusChip session={s} />
                  </span>
                  <span class="sub faint">{agentLabel(s.agent)} · {formatRelative(s.last_activity_at)}{s.origin === 'external' ? ' · external' : ''}</span>
                </a>
                {#if s.id === selectedId}
                  <Subagents parentId={s.id} count={selectedChildren} onnavigate={() => (app.sidebarOpen = false)} />
                {/if}
              </li>
            {/each}
          </ul>
        </section>
      {/each}
    </Loadable>
  </div>
</div>

<style>
  .sidebar-inner {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .search {
    padding: 12px 12px 4px;
  }
  .scroll {
    flex: 1;
    overflow: auto;
    padding: 4px 8px 16px;
  }
  .group {
    margin-top: 12px;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 4px 4px 6px;
  }
  .gname {
    font-size: 11.5px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-3);
    text-decoration: none;
  }
  .gname:hover {
    color: var(--text);
  }
  .list-plain {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 6px;
  }
  .scard {
    display: grid;
    gap: 4px;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--panel);
    text-decoration: none;
  }
  .scard:hover {
    border-color: var(--border-strong);
  }
  .scard.selected {
    background: var(--selected);
    border-color: color-mix(in srgb, var(--accent) 40%, var(--border));
  }
  .title {
    font-weight: 600;
  }
  .meta {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .where {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--text-2);
    font-size: 12.5px;
  }
  .where :global(svg) {
    flex: none;
  }
  .sub {
    font-size: 11.5px;
  }
</style>
