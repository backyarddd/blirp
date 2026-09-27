<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import LayoutGrid from '@lucide/svelte/icons/layout-grid';
  import Command from '@lucide/svelte/icons/command';
  import Settings from '@lucide/svelte/icons/settings';
  import Search from '@lucide/svelte/icons/search';
  import Coffee from '@lucide/svelte/icons/coffee';
  import logo from '../assets/logo.svg';
  import { app } from '../app.svelte';
  import { nav } from '../router.svelte';
  import { href } from '../router';
  import { isMac } from '../prefs';
  import { shortcutLabel } from '../shortcuts';
  import { updates } from '../update.svelte';

  const section = $derived(
    nav.route.name === 'projects' || nav.route.name === 'project'
      ? 'projects'
      : nav.route.name === 'sessions'
        ? 'sessions'
        : null,
  );
  const inGrid = $derived(nav.route.name === 'grid');
  // The daemon's version (the UI's own before health is known).
  const versionLabel = $derived(`blirp ${app.health?.version ?? __APP_VERSION__}`);
  const updateAvailable = $derived(updates.status?.available === true);

  // Machines holding a sleep-prevention assertion while their sessions run (sessions.keep_awake).
  const awake = $derived(
    Object.entries(app.awake)
      .filter(([, on]) => on)
      .map(([id]) => app.machineName(id)),
  );

  const connLabel = $derived.by(() => {
    const where = app.health ? `${app.health.machine.name} (${app.health.role})` : 'daemon';
    if (app.conn === 'open') return `Live updates connected to ${where}`;
    if (app.conn === 'connecting') return `Connecting to ${where}…`;
    return `Connection to ${where} lost, reconnecting…`;
  });
</script>

<header class="topbar">
  <a class="logo" href={href.sessions()} aria-label="blirp home" title={versionLabel} data-testid="logo">
    <img src={logo} width="28" height="28" alt="" />
    <span>blirp</span>
  </a>

  <nav class="pills" aria-label="Main">
    <a class="pill" href={href.projects()} aria-current={section === 'projects' ? 'page' : undefined}>Projects</a>
    <a class="pill" href={href.sessions()} aria-current={section === 'sessions' ? 'page' : undefined}>Sessions</a>
  </nav>

  {#if app.control}
  <button
    type="button"
    class="new"
    aria-label="New session"
    title="New session ({shortcutLabel('T', isMac)})"
    onclick={() => app.openNewSession()}
  >
    <Plus size={18} strokeWidth={2.5} />
  </button>
  {/if}

  <span class="spacer"></span>

  <a class="icon-btn hide-sm" href={href.search()} aria-label="Search" title="Search"><Search size={18} /></a>
  <a
    class="icon-btn hide-xs"
    class:active={inGrid}
    href={inGrid ? href.sessions() : href.grid()}
    aria-label={inGrid ? 'Leave grid view' : 'Grid view'}
    aria-current={inGrid ? 'page' : undefined}
    title="Grid view ({shortcutLabel('G', isMac)})"
  >
    <LayoutGrid size={18} />
  </a>
  {#if awake.length > 0}
    {@const text = `Keeping ${awake.join(' and ')} awake`}
    <span class="awake" role="status" aria-label={text} title="{text} while sessions run" data-testid="keep-awake">
      <Coffee size={14} aria-hidden="true" /><span class="hide-sm" aria-hidden="true">{text}</span>
    </span>
  {/if}
  <span class="conn {app.conn}" role="img" aria-label={connLabel} title={connLabel}>
    <span class="dot" aria-hidden="true"></span>
    <span class="conn-text hide-sm">{app.health?.machine.name ?? ''}</span>
  </span>
  <button
    type="button"
    class="icon-btn"
    aria-label="Command palette"
    title="Command palette ({shortcutLabel('K', isMac)})"
    onclick={() => (app.paletteOpen = true)}
  >
    <Command size={18} />
  </button>
  <a
    class="icon-btn settings"
    href={href.settings()}
    aria-label={updateAvailable ? `Settings (blirp ${updates.status?.latest} is available)` : 'Settings'}
    title={updateAvailable ? `Settings: blirp ${updates.status?.latest} is available` : 'Settings'}
    aria-current={nav.route.name === 'settings' ? 'page' : undefined}
    ><Settings size={18} />{#if updateAvailable}<span class="update-dot" aria-hidden="true" data-testid="update-dot"></span>{/if}</a
  >
</header>

<style>
  .settings {
    position: relative;
  }
  .update-dot {
    position: absolute;
    top: 5px;
    right: 5px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 2px var(--panel);
  }
  .awake {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 10px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 600;
    color: var(--accent);
    background: var(--accent-soft);
    white-space: nowrap;
  }
  .topbar {
    height: var(--topbar-h);
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 0 16px;
    background: var(--panel);
    border-bottom: 1px solid var(--border);
  }
  .logo {
    display: flex;
    align-items: center;
    gap: 8px;
    text-decoration: none;
    font-weight: 800;
    font-size: 17px;
    letter-spacing: -0.02em;
    margin-right: 8px;
  }
  .new {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    border: 0;
    background: var(--accent);
    color: #fff;
    cursor: pointer;
    box-shadow: 0 2px 6px rgb(242 84 45 / 0.35);
  }
  .new:hover {
    background: var(--accent-hover);
  }
  .icon-btn.active,
  .icon-btn[aria-current='page'] {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .conn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 0 8px;
    height: 28px;
    border-radius: 999px;
    font-size: 12px;
    color: var(--text-2);
    max-width: 180px;
  }
  .conn-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
    background: var(--completed);
  }
  .conn.connecting .dot {
    background: var(--idle);
  }
  .conn.reconnecting .dot {
    background: var(--waiting);
    animation: pulse 1.2s ease-in-out infinite;
  }
  @media (max-width: 720px) {
    .topbar {
      gap: 6px;
      padding: 0 10px;
    }
    .logo span,
    .hide-sm {
      display: none;
    }
    /* The margin sets off the wordmark, hidden here; at 390px wide fonts
       (DejaVu Sans on Linux) leave no room for it. */
    .logo {
      margin-right: 0;
    }
    .pill {
      padding: 0 9px;
    }
  }
  @media (max-width: 440px) {
    .hide-xs {
      display: none;
    }
  }
</style>
