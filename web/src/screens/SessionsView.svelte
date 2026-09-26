<script lang="ts">
  import { untrack } from 'svelte';
  import Menu from '@lucide/svelte/icons/menu';
  import { api, errorMessage } from '../lib/api/client';
  import { app } from '../lib/app.svelte';
  import { href } from '../lib/router';
  import { hasTerminal, sessionTitle } from '../lib/status';
  import SessionSidebar from '../lib/components/SessionSidebar.svelte';
  import SessionToolbar from '../lib/components/SessionToolbar.svelte';
  import MemoryPanel from '../lib/components/MemoryPanel.svelte';
  import Terminal from '../lib/terminal/Terminal.svelte';
  import SessionDetail from './SessionDetail.svelte';
  import { rememberOpenSession } from '../lib/machines';

  let { sessionId }: { sessionId: string | null } = $props();

  const session = $derived(sessionId ? app.sessionById.get(sessionId) : undefined);
  // Reopening the app (next day, after a restart) shows this session again (per browser).
  const openId = $derived(session?.id);
  $effect(() => {
    if (openId) rememberOpenSession(openId);
  });
  const project = $derived(session ? app.projectById.get(session.project_id) : undefined);

  // A terminal that was on screen stays (with its exit banner) after the process ends, until
  // the user asks for the details view or switches sessions. Resuming it remounts the pane
  // (`epoch`) so it attaches to the new process.
  let terminalFor: string | null = $state(null);
  let epoch = $state(0);
  let wasLive = false;
  $effect(() => {
    const id = sessionId;
    const live = session !== undefined && hasTerminal(session);
    untrack(() => {
      if (live) {
        if (!wasLive && terminalFor === id) epoch++;
        terminalFor = id;
      } else if (terminalFor !== id) {
        terminalFor = null;
      }
    });
    wasLive = live;
  });
  const showTerminal = $derived(session !== undefined && (hasTerminal(session) || terminalFor === session.id));

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
        {#if project}
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

    <div class="frame card">
      {#if session}
        {#if showTerminal}
          {#key `${session.id}:${epoch}`}
            <Terminal
              sessionId={session.id}
              label="Terminal: {sessionTitle(session)}"
              autofocus
              ondetails={() => (terminalFor = null)}
            />
          {/key}
        {:else}
          <SessionDetail {session} />
        {/if}
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
  }
</style>
