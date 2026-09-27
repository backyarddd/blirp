<script lang="ts">
  import { api } from '../lib/api/client';
  import type { SettingsView } from '../lib/api/types.gen';
  import { Resource } from '../lib/resource.svelte';
  import { href, type SettingsSection } from '../lib/router';
  import Loadable from '../lib/components/Loadable.svelte';
  import { app } from '../lib/app.svelte';
  import Agents from './settings/Agents.svelte';
  import MemorySettings from './settings/MemorySettings.svelte';
  import Sync from './settings/Sync.svelte';
  import Appearance from './settings/Appearance.svelte';
  import About from './settings/About.svelte';

  let { section }: { section: SettingsSection } = $props();

  const SECTIONS: { id: SettingsSection; label: string }[] = [
    { id: 'agents', label: 'Agents' },
    { id: 'memory', label: 'Memory' },
    { id: 'sync', label: 'Machines & Sync' },
    { id: 'appearance', label: 'Appearance' },
    { id: 'about', label: 'About' },
  ];

  const settings = new Resource(() => api.settings.get());
  $effect(() => {
    void settings.load();
  });

  const needsSettings = $derived(section === 'agents' || section === 'memory' || section === 'portal' || section === 'sync');

  function saved(s: SettingsView): void {
    settings.data = s;
  }
</script>

<div class="page">
  <div class="page-inner layout">
    <nav class="nav" aria-label="Settings sections">
      <h1 class="page-title">Settings</h1>
      <ul>
        {#each SECTIONS as s (s.id)}
          <li><a href={href.settings(s.id)} aria-current={s.id === (section === 'portal' ? 'sync' : section) ? 'page' : undefined}>{s.label}</a></li>
        {/each}
      </ul>
      <p class="version faint small" data-testid="settings-version">blirp {app.health?.version ?? __APP_VERSION__}</p>
    </nav>
    <div class="content">
      {#if needsSettings}
        <Loadable loading={settings.loading} error={settings.error} empty={!settings.data} onretry={() => settings.load()}>
          {#if settings.data}
            {#if section === 'agents'}
              <Agents settings={settings.data} onsaved={saved} />
            {:else if section === 'memory'}
              <MemorySettings settings={settings.data} onsaved={saved} />
            {:else}
              <!-- The LAN portal lives on the sync page; /settings/portal still lands there. -->
              <Sync settings={settings.data} onsaved={saved} />
            {/if}
          {/if}
        </Loadable>
      {:else if section === 'appearance'}
        <Appearance />
      {:else}
        <About />
      {/if}
    </div>
  </div>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 200px minmax(0, 1fr);
    gap: 24px;
    align-items: start;
  }
  .nav ul {
    list-style: none;
    margin: 12px 0 0;
    padding: 0;
    display: grid;
    gap: 2px;
  }
  .nav a {
    display: block;
    padding: 7px 10px;
    border-radius: 8px;
    text-decoration: none;
    color: var(--text-2);
    font-weight: 500;
  }
  .nav a:hover {
    background: var(--panel);
    color: var(--text);
  }
  .nav a[aria-current='page'] {
    background: var(--panel);
    color: var(--text);
    font-weight: 600;
    box-shadow: var(--shadow);
    border: 1px solid var(--border);
  }
  .version {
    margin: 12px 10px 0;
  }
  .content {
    display: grid;
    gap: 16px;
    min-width: 0;
  }
  @media (max-width: 760px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
      gap: 12px;
    }
    .nav ul {
      display: flex;
      overflow-x: auto;
      margin-top: 4px;
    }
    .nav a {
      white-space: nowrap;
    }
    .version {
      display: none;
    }
  }
</style>
