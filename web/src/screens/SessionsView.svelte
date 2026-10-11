<script lang="ts">
  import { untrack } from 'svelte';
  import Menu from '@lucide/svelte/icons/menu';
  import X from '@lucide/svelte/icons/x';
  import { ApiError, api, errorMessage } from '../lib/api/client';
  import { app } from '../lib/app.svelte';
  import { closePane, followRoute } from '../lib/panes';
  import { navigate } from '../lib/router.svelte';
  import { href } from '../lib/router';
  import { sessionTitle } from '../lib/status';
  import SessionSidebar from '../lib/components/SessionSidebar.svelte';
  import SessionToolbar from '../lib/components/SessionToolbar.svelte';
  import CompactionHint from '../lib/components/CompactionHint.svelte';
  import MemoryPanel from '../lib/components/MemoryPanel.svelte';
  import SessionPane from '../lib/components/SessionPane.svelte';
  import StatusChip from '../lib/components/StatusChip.svelte';
  import { rememberOpenSession } from '../lib/machines';

  let { sessionId }: { sessionId: string | null } = $props();

  const session = $derived(sessionId ? app.sessionById.get(sessionId) : undefined);
  // Reopening the app (next day, after a restart) shows this session again (per browser).
  const openId = $derived(session?.id);
  $effect(() => {
    if (openId) rememberOpenSession(openId);
  });
  const project = $derived(session ? app.projectById.get(session.project_id) : undefined);

  // Two or more sessions tiled side by side; the routed one is the focused pane.
  const split = $derived(sessionId !== null && app.panes.length >= 2 && app.panes.includes(sessionId));
  // A session opened while split (sidebar click) takes the focused pane's place.
  let prevRouted: string | null = null;
  $effect(() => {
    const id = sessionId;
    untrack(() => {
      if (id && app.panes.length >= 2 && !app.panes.includes(id)) app.setPanes(followRoute(app.panes, prevRouted, id));
      prevRouted = id;
    });
  });
  $effect(() => {
    const gone = app.panes.filter((id) => app.deletedSessions.has(id));
    if (gone.length) untrack(() => app.setPanes(gone.reduce(closePane, app.panes)));
  });

  // Restored panes can be older than the loaded list: read each once; one that is gone closes.
  const asked = new Set<string>();
  $effect(() => {
    if (!app.sessionsLoaded) return;
    for (const id of app.panes) {
      if (id === sessionId || app.sessionById.has(id) || asked.has(id)) continue;
      asked.add(id);
      api.sessions.get(id).then(
        ({ children_count, ...s }) => {
          void children_count;
          app.upsertSession(s);
        },
        (e: unknown) => {
          if (e instanceof ApiError && e.status === 404) app.setPanes(closePane(app.panes, id));
          else console.warn(`blirp: could not read session ${id}`, e);
        },
      );
    }
  });

  function focusPane(id: string): void {
    if (id !== sessionId) navigate(href.sessions(id), { replace: true });
  }
  function closeSplitPane(id: string): void {
    const before = app.panes;
    app.setPanes(closePane(before, id));
    if (id === sessionId) navigate(href.sessions(app.panes[0] ?? before.find((x) => x !== id) ?? null));
  }

  // The detail carries the subagent count (lists leave subagents out); it is re-read when the
  // status changes, since subagents are ingested while the parent works. Sessions older than
  // the sidebar window come from here too.
  let missing: string | null = $state(null);
  let childrenCount = $state(0);
  const status = $derived(session?.status);
  $effect(() => {
    const id = sessionId;
    void status;
    if (!id || !app.sessionsLoaded) return;
    untrack(() => {
      missing = null;
      api.sessions
        .get(id)
        .then(({ children_count, ...s }) => {
          if (id !== sessionId) return;
          childrenCount = children_count;
          if (!app.sessionById.has(id)) app.upsertSession(s);
        })
        .catch((e: unknown) => {
          if (id !== sessionId) return;
          if (app.sessionById.has(id)) console.warn(`blirp: could not read session ${id}`, e);
          else missing = errorMessage(e);
        });
    });
  });
  $effect(() => {
    void sessionId;
    childrenCount = 0;
  });
</script>

<div class="layout" class:with-memory={session && app.memoryPanel}>
  <aside class="sidebar" class:open={app.sidebarOpen} aria-label="Sessions">
    <SessionSidebar selectedId={sessionId} selectedChildren={childrenCount} />
  </aside>
  {#if app.sidebarOpen}
    <button type="button" class="scrim" aria-label="Close sessions list" onclick={() => (app.sidebarOpen = false)}></button>
  {/if}

  <section class="main">
    <div class="bar">
      <button type="button" class="icon-btn drawer-btn" aria-label="Show sessions list" onclick={() => (app.sidebarOpen = true)}>
        <Menu size={18} />
      </button>
      <nav class="crumbs ellipsis" aria-label="Breadcrumb">
        {#if project?.chats}
          <span>Chats</span>
        {:else if project}
          <a href={href.project(project.id)}>{project.name}</a>
        {:else}
          <span>All projects</span>
        {/if}
        <span class="sep" aria-hidden="true">/</span>
        <a href={href.sessions()} aria-current={session ? undefined : 'page'}>Sessions</a>
        {#if session}
          <span class="sep" aria-hidden="true">/</span>
          <span class="current ellipsis" aria-current="page">{sessionTitle(session)}</span>
        {/if}
      </nav>
      <span class="spacer"></span>
      {#if session}<SessionToolbar {session} />{/if}
    </div>
    {#if session}
      {#key session.id}<CompactionHint {session} />{/key}
    {/if}

    <div class="frame card" class:split>
      {#if split}
        <div class="panes" style:--cols={app.panes.length === 4 ? 2 : app.panes.length}>
          {#each app.panes as id (id)}
            {@const ps = app.sessionById.get(id)}
            <section
              class="pane"
              class:focused={id === sessionId}
              aria-label={ps ? sessionTitle(ps) : 'Session'}
              onfocusin={() => focusPane(id)}
              onpointerdown={() => focusPane(id)}
            >
              <header>
                <span class="ptitle ellipsis">{ps ? sessionTitle(ps) : 'Loading…'}</span>
                {#if ps}
                  <span class="faint small ellipsis">{app.projectLabel(ps.project_id)}</span>
                  <span class="spacer"></span>
                  <StatusChip session={ps} />
                {:else}
                  <span class="spacer"></span>
                {/if}
                <button
                  type="button"
                  class="icon-btn sm"
                  aria-label="Close pane"
                  title="Close pane"
                  onpointerdown={(e) => e.stopPropagation()}
                  onclick={(e) => {
                    e.stopPropagation();
                    closeSplitPane(id);
                  }}><X size={14} /></button
                >
              </header>
              {#if ps}
                <SessionPane session={ps} autofocus={id === sessionId} />
              {:else}
                <div class="empty" role="status"><p class="muted">{id === sessionId && missing ? missing : 'Loading session…'}</p></div>
              {/if}
            </section>
          {/each}
        </div>
      {:else if session}
        <SessionPane {session} autofocus />
      {:else if sessionId && missing}
        <div class="empty">
          <h2>Session unavailable</h2>
          <p class="muted">{missing}</p>
          <a class="btn" href={href.sessions()}>Back to sessions</a>
        </div>
      {:else if sessionId}
        <div class="empty" role="status"><p class="muted">Loading session…</p></div>
      {:else}
        <div class="empty">
          <h2>Pick a session</h2>
          <p class="muted">
            Choose a session on the left, or start a new one. Every new session starts with this project's memory.
          </p>
          {#if app.control}<button type="button" class="btn primary" onclick={() => app.openNewSession()}>New session</button>{/if}
        </div>
      {/if}
    </div>
  </section>

  {#if session && app.memoryPanel}
    <button type="button" class="scrim memory-scrim" aria-label="Close memory panel" onclick={() => app.setMemoryPanel(false)}></button>
    <aside class="memory" aria-label="Memory">
      {#key session.id}<MemoryPanel {session} />{/key}
    </aside>
  {/if}
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 300px minmax(0, 1fr);
    height: 100%;
  }
  .layout.with-memory {
    grid-template-columns: 300px minmax(0, 1fr) 340px;
  }
  .sidebar {
    min-height: 0;
    border-right: 1px solid var(--border);
    background: var(--bg);
  }
  .main {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
    min-height: 0;
    padding: 12px 16px 16px;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    min-height: 34px;
  }
  .crumbs {
    display: flex;
    align-items: center;
    gap: 6px;
    font-weight: 600;
    min-width: 0;
  }
  .crumbs a {
    text-decoration: none;
    color: var(--text-2);
  }
  .crumbs a:hover {
    color: var(--text);
  }
  .crumbs .current {
    color: var(--text);
  }
  .sep {
    color: var(--text-3);
  }
  .frame {
    flex: 1;
    min-height: 0;
    overflow: hidden;
    position: relative;
  }
  .frame.split {
    background: none;
    border: 0;
    box-shadow: none;
    overflow: visible;
  }
  .panes {
    display: grid;
    grid-template-columns: repeat(var(--cols), minmax(0, 1fr));
    grid-auto-rows: minmax(0, 1fr);
    gap: 10px;
    height: 100%;
  }
  .pane {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    overflow: hidden;
  }
  .pane.focused {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-soft);
  }
  .pane > header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 6px 4px 10px;
    border-bottom: 1px solid var(--border);
    min-width: 0;
    font-size: 12.5px;
  }
  .ptitle {
    font-weight: 600;
  }
  .pane > :global(.pane-body) {
    flex: 1;
  }
  .memory {
    min-height: 0;
    border-left: 1px solid var(--border);
    background: var(--panel);
  }
  .empty {
    display: grid;
    place-items: center;
    align-content: center;
    gap: 6px;
    height: 100%;
    padding: 24px;
    text-align: center;
  }
  .empty h2 {
    margin: 0;
    font-size: 17px;
  }
  .empty p {
    max-width: 420px;
    margin: 0 0 8px;
  }
  .drawer-btn,
  .scrim {
    display: none;
  }
  .scrim {
    position: fixed;
    inset: var(--topbar-h) 0 0 0;
    background: rgb(15 17 21 / 0.35);
    border: 0;
    z-index: 25;
  }

  @media (max-width: 1100px) {
    .memory-scrim {
      display: block;
    }
    .layout.with-memory {
      grid-template-columns: 260px minmax(0, 1fr);
    }
    .memory {
      position: fixed;
      right: 0;
      top: var(--topbar-h);
      bottom: 0;
      width: min(360px, 92vw);
      z-index: 30;
      box-shadow: var(--shadow-lg);
    }
    .layout {
      grid-template-columns: 260px minmax(0, 1fr);
    }
  }

  @media (max-width: 800px) {
    .layout,
    .layout.with-memory {
      grid-template-columns: minmax(0, 1fr);
    }
    .drawer-btn {
      display: inline-grid;
    }
    .sidebar {
      position: fixed;
      top: var(--topbar-h);
      bottom: 0;
      left: 0;
      width: min(320px, 86vw);
      z-index: 40;
      transform: translateX(-100%);
      visibility: hidden; /* keeps the closed drawer out of the tab order */
      transition:
        transform 0.18s ease,
        visibility 0.18s;
      box-shadow: var(--shadow-lg);
    }
    .sidebar.open {
      transform: none;
      visibility: visible;
    }
    .scrim {
      display: block;
      z-index: 35;
    }
    .main {
      padding: 8px;
    }
    .frame {
      border-radius: 8px;
    }
    .panes {
      grid-template-columns: minmax(0, 1fr);
    }
    .pane:not(.focused) {
      display: none;
    }
  }
</style>
