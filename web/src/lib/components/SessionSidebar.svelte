<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import Folder from '@lucide/svelte/icons/folder';
  import Pin from '@lucide/svelte/icons/pin';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import ListChecks from '@lucide/svelte/icons/list-checks';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import ChevronDown from '@lucide/svelte/icons/chevron-down';
  import { untrack } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import { api, errorMessage } from '../api/client';
  import type { ProjectSummary, Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { href } from '../router';
  import {
    GROUP_PREVIEW,
    agentLabel,
    basename,
    groupSessions,
    isPinned as trusted,
    isRecent,
    previewSessions,
    sessionOrder,
    sessionTitle,
    type LiveContext,
  } from '../status';
  import { formatRelative } from '../time';
  import { projectActions, sessionActions } from '../actions';
  import { actionEnv, projectOps, sessionOps } from '../manage';
  import { contextmenu } from '../contextmenu';
  import { markedFirst, parseMarks, projectKey, sessionArchived, sessionKey } from '../marks';
  import { readPref, writePref } from '../prefs';
  import { Selection } from '../selection.svelte';
  import StatusChip from './StatusChip.svelte';
  import Loadable from './Loadable.svelte';
  import Subagents from './Subagents.svelte';
  import MachineBadge from './MachineBadge.svelte';
  import Menu from './Menu.svelte';
  import BulkBar from './BulkBar.svelte';

  /** `selectedChildren`: subagent count of the selected session (lists leave subagents out). */
  let { selectedId, selectedChildren }: { selectedId: string | null; selectedChildren: number } = $props();

  const PAGE = 100;

  let filter = $state('');
  let machine = $state('');
  const expanded = new SvelteSet<string>();

  // Recent hides ended sessions older than RECENT_MS; "All + archived" is the old "Show archived".
  // Archived follows the app-wide toggle (palette, other lists); the recency cut is this list's own.
  type View = 'recent' | 'all' | 'archived';
  const VIEWS: readonly View[] = ['recent', 'all', 'archived'];
  let older: 'recent' | 'all' = $state(readPref('blirp.sidebar.view', ['recent', 'all'], 'recent'));
  const view: View = $derived(app.showArchived ? 'archived' : older);
  function setView(v: View): void {
    app.showArchived = v === 'archived';
    if (v !== 'archived') {
      older = v;
      writePref('blirp.sidebar.view', v);
    }
  }

  function readCollapsed(): string[] {
    try {
      return parseMarks(localStorage.getItem('blirp.sidebar.collapsed'));
    } catch {
      return [];
    }
  }
  // A collapsed group shows only its live, pinned and selected sessions.
  const collapsed = new SvelteSet<string>(readCollapsed());
  function toggleCollapsed(key: string): void {
    if (!collapsed.delete(key)) collapsed.add(key);
    writePref('blirp.sidebar.collapsed', JSON.stringify([...collapsed]));
  }
  const selection = new Selection();

  const q = $derived(filter.trim());
  const filtering = $derived(q !== '' || machine !== '');
  const matches = (s: Session): boolean =>
    (machine === '' || s.machine_id === machine) &&
    (q === '' || `${sessionTitle(s)} ${s.branch ?? ''} ${s.cwd} ${s.agent}`.toLowerCase().includes(q.toLowerCase()));
  // Archived sessions (and those of archived projects) stay out unless asked for.
  const visible = (s: Session): boolean => app.showArchived || !sessionArchived(s, app.archived, app.projectById);
  const pinned = (s: Session): boolean => app.pinned.has(sessionKey(s.id));

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

  // Searching looks at everything; only the unfiltered Recent view cuts old ended sessions.
  const inRecent = (s: Session, ctx: LiveContext): boolean => isRecent(s, ctx) || pinned(s) || s.id === selectedId;
  const hiddenOld = $derived.by(() => {
    if (filtering || view !== 'recent') return false;
    const ctx = app.liveContext();
    return app.topSessions.some((s) => visible(s) && !inRecent(s, ctx));
  });

  // Pinned sessions first; grouping keeps that order inside each project.
  const list = $derived.by(() => {
    if (!filtering) {
      const ctx = app.liveContext();
      return markedFirst(
        app.topSessions.filter((s) => visible(s) && (view !== 'recent' || inRecent(s, ctx))),
        pinned,
      );
    }
    // Results take the live copy when this client has one; new sessions matching the filter
    // appear without asking the daemon again.
    const byId = new Map<string, Session>();
    for (const s of found) byId.set(s.id, app.sessionById.get(s.id) ?? s);
    for (const s of app.topSessions) if (matches(s)) byId.set(s.id, s);
    const hits = [...byId.values()].filter((s) => !app.deletedSessions.has(s.id) && matches(s) && visible(s));
    return markedFirst(hits.sort(sessionOrder(app.liveContext())), pinned);
  });

  /** The Chats group's menu acts on this machine's bucket, else on one of its sessions' buckets. */
  function chatsProject(sessions: readonly Session[]): ProjectSummary | undefined {
    return app.projects.find((p) => p.chats && p.is_home) ?? app.projectById.get(sessions[0]?.project_id ?? '');
  }

  const groups = $derived.by(() => {
    const ctx = app.liveContext();
    const key = (projectId: string | null): string => (projectId === null ? projectKey({ id: 'chats', chats: true }) : projectKey({ id: projectId }));
    const all = groupSessions(list, app.projectById).filter((g) => app.showArchived || !app.archived.has(key(g.projectId)));
    return markedFirst(all, (g) => app.pinned.has(key(g.projectId))).map((g) => ({
      ...g,
      pinned: app.pinned.has(key(g.projectId)),
      menuProject: g.projectId === null ? chatsProject(g.sessions) : g.project,
      open: expanded.has(g.key),
      collapsed: collapsed.has(g.key),
      running: g.sessions.filter((s) => trusted(s, ctx)).length,
      preview: collapsed.has(g.key)
        ? { shown: g.sessions.filter((s) => s.id === selectedId || trusted(s, ctx) || pinned(s)), hidden: 0 }
        : previewSessions(g.sessions, expanded.has(g.key) ? Infinity : GROUP_PREVIEW, selectedId, ctx, pinned),
    }));
  });
  const shown = $derived(groups.flatMap((g) => g.preview.shown));
  // Previous/next session shortcuts walk the cards as shown here.
  $effect(() => {
    app.sidebarOrder = shown.map((s) => s.id);
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
    <div class="tools">
      <select class="select sm" aria-label="Which sessions" value={view}
        onchange={(e) => setView(VIEWS.find((v) => v === e.currentTarget.value) ?? 'recent')}>
        <option value="recent">Recent</option>
        <option value="all">All</option>
        <option value="archived">All + archived</option>
      </select>
      {#if !selection.active}
        <button type="button" class="btn ghost sm" onclick={() => selection.start()}><ListChecks size={14} aria-hidden="true" />Select</button>
      {/if}
    </div>
    {#if selection.active}<BulkBar {selection} {shown} />{/if}
  </div>
  <div class="scroll">
    <Loadable
      loading={!app.sessionsLoaded || (filtering && foundLoading && list.length === 0)}
      error={filtering ? (list.length === 0 ? foundError : null) : app.sessionsError}
      empty={groups.length === 0}
      emptyText={filtering
        ? 'No sessions match.'
        : hiddenOld
          ? 'Nothing in the last 3 days.'
          : 'No sessions yet. Start one and it shows up here, along with sessions you run outside blirp.'}
      onretry={() => (filtering ? search(true) : app.refreshSessions())}
    >
      {#snippet emptyAction()}
        {#if !filtering && hiddenOld}
          <button class="btn sm" type="button" onclick={() => setView('all')}>Show all</button>
        {:else if !filtering && app.control}
          <button class="btn primary sm" type="button" onclick={() => app.openNewSession()}>New session</button>
        {/if}
      {/snippet}
      {#each groups as g (g.key)}
        {@const gp = g.menuProject}
        <section class="group" aria-label={g.name}>
          <header use:contextmenu={{ items: () => (gp ? projectActions(gp, actionEnv(), projectOps) : []), label: g.name }}>
            <button
              type="button"
              class="icon-btn sm chev"
              aria-label={g.collapsed ? `Expand ${g.name}` : `Collapse ${g.name}`}
              aria-expanded={!g.collapsed}
              onclick={() => toggleCollapsed(g.key)}
            >
              {#if g.collapsed}<ChevronRight size={13} />{:else}<ChevronDown size={13} />{/if}
            </button>
            {#if g.projectId !== null && g.project}
              <a class="gname ellipsis" href={href.project(g.projectId)}>{g.name}</a>
            {:else}
              <!-- Chats, or a project this machine does not know (yet): nothing to link to. -->
              <span class="gname ellipsis" title={g.projectId === null ? 'Sessions that belong to no project' : 'A project this machine has not received yet'}
                >{g.name}</span
              >
            {/if}
            {#if g.pinned}<Pin size={11} class="pin-mark" aria-label="Pinned" />{/if}
            {#if g.running > 0}<span class="running faint small">{g.running} running</span>{/if}
            <span class="spacer"></span>
            {#if gp}
              <Menu items={projectActions(gp, actionEnv(), projectOps)} label="Actions for {g.name}" title="More actions" triggerClass="icon-btn sm" align="right"
                ><Ellipsis size={14} /></Menu
              >
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
              {@const isPinned = pinned(s)}
              {@const isArchived = sessionArchived(s, app.archived, app.projectById)}
              <li class="item" class:selecting={selection.active}>
                {#if selection.active}
                  <input
                    type="checkbox"
                    class="pick"
                    aria-label="Select {sessionTitle(s)}"
                    checked={selection.ids.has(s.id)}
                    onclick={(e) =>
                      selection.toggle(
                        s.id,
                        e.shiftKey,
                        shown.map((x) => x.id),
                      )}
                  />
                {/if}
                <a
                  class="scard"
                  class:selected={s.id === selectedId}
                  class:archived={isArchived}
                  href={href.sessions(s.id)}
                  aria-current={s.id === selectedId ? 'page' : undefined}
                  onclick={() => (app.sidebarOpen = false)}
                  use:contextmenu={{
                    items: () => sessionActions(s, actionEnv(), sessionOps),
                    label: sessionTitle(s),
                    rename: app.control ? () => sessionOps.rename(s) : undefined,
                  }}
                >
                  <span class="title ellipsis"
                    >{#if isPinned}<Pin size={11} class="pin-mark" aria-label="Pinned" />{/if}{sessionTitle(s)}</span
                  >
                  <span class="sub">
                    <span class="where faint">
                      {#if s.branch}
                        <GitBranch size={12} aria-label="Branch" />
                      {:else}
                        <Folder size={12} aria-label="Folder" />
                      {/if}
                      <span class="ellipsis"
                        >{s.branch ?? basename(s.cwd)} · {agentLabel(s.agent)} · {formatRelative(s.last_activity_at)}{s.origin === 'external'
                          ? ' · external'
                          : ''}{isArchived ? ' · archived' : ''}</span
                      >
                    </span>
                    <span class="right"><MachineBadge machineId={s.machine_id} /><StatusChip session={s} /></span>
                  </span>
                </a>
                <span class="more">
                  <Menu
                    items={sessionActions(s, actionEnv(), sessionOps)}
                    label="Actions for {sessionTitle(s)}"
                    title="More actions"
                    triggerClass="icon-btn sm"
                    align="right"><Ellipsis size={14} /></Menu
                  >
                </span>
                {#if s.id === selectedId}
                  <Subagents parentId={s.id} count={selectedChildren} onnavigate={() => (app.sidebarOpen = false)} />
                {/if}
              </li>
            {/each}
          </ul>
          {#if g.collapsed}
            <!-- Collapsed groups list only live, pinned and selected sessions; there is nothing to expand here. -->
          {:else if g.preview.hidden > 0}
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
          class="btn sm more-btn"
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
  .tools {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    min-height: 28px;
  }
  .tools .select {
    width: auto;
    min-height: 28px;
    padding: 2px 8px;
    font-size: 12.5px;
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
  .more-btn {
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
    gap: 2px;
    padding: 0 4px 4px 6px;
  }
  .gname {
    font-size: 11.5px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-3);
    text-decoration: none;
    min-width: 0;
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
  .item {
    position: relative;
    min-width: 0;
  }
  .item.selecting {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    align-items: start;
    column-gap: 6px;
  }
  .item.selecting > :global(*:not(.pick):not(.more)) {
    grid-column: 2;
  }
  .pick {
    margin-top: 14px;
  }
  /* The "⋯" button sits on the card's top right: shown on hover and focus, always on touch screens. */
  .more {
    position: absolute;
    top: 6px;
    right: 6px;
    opacity: 0;
  }
  .item:hover .more,
  .more:focus-within,
  .scard:focus-visible + .more {
    opacity: 1;
  }
  @media (hover: none) {
    .more {
      opacity: 1;
    }
  }
  .scard {
    display: grid;
    gap: 3px;
    padding: 8px 10px;
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
  .scard.archived {
    opacity: 0.7;
  }
  .title {
    font-weight: 600;
    padding-right: 24px;
  }
  .where {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
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
    font-size: 12px;
  }
  .right {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    flex: none;
  }
  .running {
    margin-left: 4px;
    white-space: nowrap;
  }
  :global(.pin-mark) {
    flex: none;
    margin-right: 4px;
    color: var(--accent);
    vertical-align: -1px;
  }
</style>
