<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import { untrack } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import { api, errorMessage } from '../api/client';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { href } from '../router';
  import { GROUP_PREVIEW, agentLabel, basename, groupSessions, previewSessions, sessionOrder, sessionTitle } from '../status';
  import { formatRelative } from '../time';
  import StatusChip from './StatusChip.svelte';
  import Loadable from './Loadable.svelte';
  import Subagents from './Subagents.svelte';
  import MachineBadge from './MachineBadge.svelte';

  /** `selectedChildren`: subagent count of the selected session (lists leave subagents out). */
  let { selectedId, selectedChildren }: { selectedId: string | null; selectedChildren: number } = $props();

  const PAGE = 100;

  let filter = $state('');
  let machine = $state('');
  const expanded = new SvelteSet<string>();

  const q = $derived(filter.trim());
  const filtering = $derived(q !== '' || machine !== '');
  const matches = (s: Session): boolean =>
    (machine === '' || s.machine_id === machine) &&
    (q === '' || `${sessionTitle(s)} ${s.branch ?? ''} ${s.cwd} ${s.agent}`.toLowerCase().includes(q.toLowerCase()));

  // A filter asks the daemon, so sessions beyond the pages loaded here are found too.
  let found: Session[] = $state.raw([]);
  let foundCursor: string | null = $state(null);
  let foundLoading = $state(false);
  let foundError: string | null = $state(null);
  let token = 0;

  async function search(reset: boolean): Promise<void> {
    const t = ++token;
    foundLoading = true;
    foundError = null;
    try {
      const page = await api.sessions.list({
        ...(q ? { q } : {}),
        ...(machine ? { machine } : {}),
        ...(!reset && foundCursor ? { cursor: foundCursor } : {}),
        limit: PAGE,
      });
      if (t !== token) return;
      found = reset ? page.items : [...found, ...page.items];
      foundCursor = page.next_cursor;
    } catch (e) {
      if (t === token) foundError = errorMessage(e);
    } finally {
      if (t === token) foundLoading = false;
    }
  }

  $effect(() => {
    const active = filtering;
    void q;
    void machine;
    untrack(() => {
      token++;
      found = [];
      foundCursor = null;
      foundError = null;
      foundLoading = active;
    });
    if (!active) return;
    const timer = setTimeout(() => void search(true), 250);
    return () => clearTimeout(timer);
  });

  const list = $derived.by(() => {
    if (!filtering) return app.topSessions;
    // Results take the live copy when this client has one; new sessions matching the filter
    // appear without asking the daemon again.
    const byId = new Map<string, Session>();
    for (const s of found) byId.set(s.id, app.sessionById.get(s.id) ?? s);
    for (const s of app.topSessions) if (matches(s)) byId.set(s.id, s);
    return [...byId.values()].filter((s) => !app.deletedSessions.has(s.id) && matches(s)).sort(sessionOrder(app.liveContext()));
  });
  const groups = $derived.by(() => {
    const ctx = app.liveContext();
    return groupSessions(list, app.projectById).map((g) => ({
      ...g,
      open: expanded.has(g.key),
      preview: previewSessions(g.sessions, expanded.has(g.key) ? Infinity : GROUP_PREVIEW, selectedId, ctx),
    }));
  });
  // Previous/next session shortcuts walk the cards as shown here.
  $effect(() => {
    app.sidebarOrder = groups.flatMap((g) => g.preview.shown.map((s) => s.id));
  });
  $effect(() => () => {
    app.sidebarOrder = [];
  });
  const more = $derived(filtering ? foundCursor !== null : app.sessionsCursor !== null);
  const loadingMore = $derived(filtering ? foundLoading && found.length > 0 : app.sessionsLoadingMore);
  const machines = $derived(app.machines.length > 1 ? app.machines : []);
</script>

<div class="sidebar-inner">
  <div class="search">
    <input class="input" type="search" placeholder="Filter sessions" aria-label="Filter sessions" bind:value={filter} />
    {#if machines.length > 0}
      <select class="select" aria-label="Machine" bind:value={machine}>
        <option value="">All machines</option>
        {#each machines as m (m.id)}
          <option value={m.id}>{app.machineName(m.id)}{m.id === app.selfId ? ' (this machine)' : ''}</option>
        {/each}
      </select>
    {/if}
  </div>
  <div class="scroll">
    <Loadable
      loading={!app.sessionsLoaded || (filtering && foundLoading && list.length === 0)}
      error={filtering ? (list.length === 0 ? foundError : null) : app.sessionsError}
      empty={groups.length === 0}
      emptyText={filtering ? 'No sessions match.' : 'No sessions yet. Start one and it shows up here, along with sessions you run outside blirp.'}
      onretry={() => (filtering ? search(true) : app.refreshSessions())}
    >
      {#snippet emptyAction()}
        {#if !filtering && app.control}<button class="btn primary sm" type="button" onclick={() => app.openNewSession()}>New session</button>{/if}
      {/snippet}
      {#each groups as g (g.key)}
        <section class="group" aria-label={g.name}>
          <header>
            {#if g.projectId === null}
              <span class="gname ellipsis" title="Sessions that belong to no project">{g.name}</span>
            {:else}
              <a class="gname ellipsis" href={href.project(g.projectId)}>{g.name}</a>
            {/if}
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
            {#each g.preview.shown as s (s.id)}
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
                  <span class="sub">
                    <span class="faint ellipsis">{agentLabel(s.agent)} · {formatRelative(s.last_activity_at)}{s.origin === 'external' ? ' · external' : ''}</span>
                    <MachineBadge machineId={s.machine_id} />
                  </span>
                </a>
                {#if s.id === selectedId}
                  <Subagents parentId={s.id} count={selectedChildren} onnavigate={() => (app.sidebarOpen = false)} />
                {/if}
              </li>
            {/each}
          </ul>
          {#if g.preview.hidden > 0}
            <button type="button" class="link-btn" onclick={() => expanded.add(g.key)}>Show {g.preview.hidden} more</button>
          {:else if g.open && g.sessions.length > GROUP_PREVIEW}
            <button type="button" class="link-btn" onclick={() => expanded.delete(g.key)}>Show fewer</button>
          {/if}
        </section>
      {/each}
      {#if filtering && foundError && list.length > 0}<p class="err" role="alert">{foundError}</p>{/if}
      {#if more}
        <button
          type="button"
          class="btn sm more"
          disabled={loadingMore}
          onclick={() => (filtering ? search(false) : app.loadMoreSessions())}>{loadingMore ? 'Loading…' : 'Load older sessions'}</button
        >
      {/if}
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
    display: grid;
    gap: 6px;
    padding: 12px 12px 4px;
  }
  .link-btn {
    margin: 4px 0 0 6px;
    padding: 2px 0;
    border: 0;
    background: none;
    color: var(--text-2);
    font-size: 12px;
    cursor: pointer;
  }
  .link-btn:hover {
    color: var(--text);
    text-decoration: underline;
  }
  .more {
    width: 100%;
    margin-top: 12px;
  }
  .err {
    color: var(--danger);
    font-size: 12px;
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
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    min-width: 0;
    font-size: 11.5px;
  }
</style>
