<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { app } from './lib/app.svelte';
  import { nav, navigate, startRouter } from './lib/router.svelte';
  import { href } from './lib/router';
  import { applyTheme } from './lib/theme.svelte';
  import { matchShortcut } from './lib/shortcuts';
  import { isMac } from './lib/prefs';
  import { groupSessions } from './lib/status';
  import { forgetOpenSession } from './lib/machines';
  import TopBar from './lib/components/TopBar.svelte';
  import FilesBanner from './lib/components/FilesBanner.svelte';
  import CommandPalette from './lib/components/CommandPalette.svelte';
  import NewSessionDialog from './lib/components/NewSessionDialog.svelte';
  import Toasts from './lib/components/Toasts.svelte';
  import ContextMenuHost from './lib/components/ContextMenuHost.svelte';
  import ActionDialogs from './lib/components/ActionDialogs.svelte';
  import UpdateBanner from './lib/components/UpdateBanner.svelte';
  import logo from './lib/assets/logo.svg';
  import SessionsView from './screens/SessionsView.svelte';
  import GridView from './screens/GridView.svelte';
  import ProjectsView from './screens/ProjectsView.svelte';
  import ProjectView from './screens/ProjectView.svelte';
  import TrashView from './screens/TrashView.svelte';
  import SearchView from './screens/SearchView.svelte';
  import SettingsView from './screens/SettingsView.svelte';

  applyTheme();

  onMount(() => {
    const stopRouter = startRouter();
    void app.boot();
    return () => {
      stopRouter();
      app.stopStream();
    };
  });

  const route = $derived(nav.route);

  // Opening a session that asked for attention clears it from the title and favicon badge.
  $effect(() => {
    void nav.route;
    untrack(() => app.markSeen());
  });

  function currentProjectId(): string | null {
    if (route.name === 'project') return route.projectId;
    if (route.name === 'sessions' && route.sessionId) return app.sessionById.get(route.sessionId)?.project_id ?? null;
    return null;
  }

  function switchSession(delta: number): void {
    // The cards the sidebar shows, in its order; every session when the sidebar is not on screen.
    const order =
      app.sidebarOrder.length > 0
        ? app.sidebarOrder
        : groupSessions(app.topSessions, app.projectById).flatMap((g) => g.sessions.map((s) => s.id));
    if (order.length === 0) return;
    const current = route.name === 'sessions' ? route.sessionId : null;
    const i = current === null ? -1 : order.indexOf(current);
    const next = order[i < 0 ? 0 : (i + delta + order.length) % order.length];
    if (next) navigate(href.sessions(next));
  }

  function onKey(e: KeyboardEvent): void {
    if (app.auth !== 'ok' || e.defaultPrevented) return;
    const inTerminal = e.target instanceof Element && e.target.closest('.xterm') !== null;
    const action = matchShortcut(e, isMac, inTerminal);
    if (!action) return;
    // Arrow chords keep their text-editing meaning inside form fields.
    const inField =
      !inTerminal && e.target instanceof HTMLElement && e.target.matches('input, textarea, select, [contenteditable]');
    if (inField && (action === 'prev' || action === 'next')) return;
    e.preventDefault();
    switch (action) {
      case 'palette':
        app.paletteOpen = !app.paletteOpen;
        break;
      case 'new':
        app.openNewSession(currentProjectId());
        break;
      case 'grid':
        navigate(route.name === 'grid' ? href.sessions() : href.grid());
        break;
      case 'prev':
        switchSession(-1);
        break;
      case 'next':
        switchSession(1);
        break;
      case 'close':
        if (route.name === 'sessions' && route.sessionId) {
          forgetOpenSession(route.sessionId);
          navigate(href.sessions());
        }
        break;
    }
  }
</script>

<svelte:window onkeydown={onKey} />

{#if app.auth === 'ok'}
  <div class="shell">
    <TopBar />
    <div class="banners">
      <UpdateBanner />
      <FilesBanner />
    </div>
    <main class="main">
      {#if route.name === 'sessions'}
        <SessionsView sessionId={route.sessionId} />
      {:else if route.name === 'grid'}
        <GridView />
      {:else if route.name === 'projects'}
        <ProjectsView />
      {:else if route.name === 'trash'}
        <TrashView />
      {:else if route.name === 'project'}
        <ProjectView projectId={route.projectId} tab={route.tab} sub={route.sub} />
      {:else if route.name === 'search'}
        <SearchView q={route.q} project={route.project} kind={route.kind} />
      {:else if route.name === 'settings'}
        <SettingsView section={route.section} />
      {:else}
        <div class="page">
          <div class="center card panel-pad">
            <h1 class="page-title">Page not found</h1>
            <p class="muted">Nothing lives at <code>{route.path}</code>.</p>
            <a class="btn" href={href.sessions()}>Go to sessions</a>
          </div>
        </div>
      {/if}
    </main>
  </div>
  <CommandPalette />
  <NewSessionDialog />
  <ActionDialogs />
  <ContextMenuHost />
{:else}
  <div class="gate">
    <div class="card gate-card">
      <img src={logo} width="48" height="48" alt="" />
      {#if app.auth === 'checking'}
        <h1>Connecting to blirp…</h1>
        <p class="muted" role="status">Talking to the local daemon.</p>
      {:else if app.auth === 'unauthorized'}
        <h1>Sign in required</h1>
        <p>
          Open blirp from the desktop app or run <code>blirp open</code> in a terminal. Both sign this browser in; the
          sign-in ends when the daemon restarts. In the desktop app, Try again signs the window in again.
        </p>
        <p class="muted small">On another device, scan the login QR code from Settings &gt; Machines &amp; Sync on an already signed-in screen.</p>
        <!-- A reload: the desktop app signs the window in again when a page loads. -->
        <button class="btn" type="button" onclick={() => location.reload()}>Try again</button>
      {:else}
        <h1>Cannot reach the blirp daemon</h1>
        <p>Start it with <code>blirp daemon</code> (or open the desktop app), then try again.</p>
        {#if app.bootError}<p class="muted small mono">{app.bootError}</p>{/if}
        <button class="btn primary" type="button" onclick={() => app.boot()}>Try again</button>
      {/if}
    </div>
  </div>
{/if}
<Toasts />

<style>
  .shell {
    display: grid;
    /* The middle row holds the update and file sync banners; empty without them. */
    grid-template-rows: var(--topbar-h) auto minmax(0, 1fr);
    height: 100%;
  }
  .main {
    grid-row: 3;
    min-height: 0;
    overflow: hidden;
  }
  .center {
    max-width: 520px;
    margin: 10vh auto 0;
  }
  .gate {
    min-height: 100%;
    display: grid;
    place-items: center;
    padding: 16px;
  }
  .gate-card {
    max-width: 460px;
    padding: 32px;
    text-align: center;
  }
  .gate-card h1 {
    font-size: 20px;
    margin: 16px 0 8px;
  }
</style>
