<script lang="ts">
  import { app } from '../app.svelte';
  import { nav, navigate } from '../router.svelte';
  import { href } from '../router';
  import { agentLabel, sessionTitle, statusInfo } from '../status';
  import { toggleTheme } from '../theme.svelte';
  import { isMac } from '../prefs';
  import { shortcutLabel } from '../shortcuts';

  interface Item {
    id: string;
    group: 'Actions' | 'Sessions' | 'Projects';
    label: string;
    hint: string;
    run: () => void;
  }

  let dialog: HTMLDialogElement | undefined = $state();
  let input: HTMLInputElement | undefined = $state();
  let query = $state('');
  let active = $state(0);

  $effect(() => {
    if (!dialog) return;
    if (app.paletteOpen && !dialog.open) {
      query = '';
      active = 0;
      dialog.showModal();
      input?.focus();
    } else if (!app.paletteOpen && dialog.open) {
      dialog.close();
    }
  });

  const go = (to: string) => () => navigate(to);

  const items: Item[] = $derived.by(() => {
    const q = query.trim();
    const actions: Item[] = [
      { id: 'a:new', group: 'Actions', label: 'New session', hint: shortcutLabel('T', isMac), run: () => app.openNewSession() },
      ...(q ? [{ id: 'a:search', group: 'Actions' as const, label: `Search memory for "${q}"`, hint: '', run: go(href.search(q)) }] : []),
      {
        id: 'a:grid',
        group: 'Actions',
        label: nav.route.name === 'grid' ? 'Leave grid view' : 'Grid view',
        hint: shortcutLabel('G', isMac),
        run: go(nav.route.name === 'grid' ? href.sessions() : href.grid()),
      },
      { id: 'a:projects', group: 'Actions', label: 'Go to projects', hint: '', run: go(href.projects()) },
      { id: 'a:sessions', group: 'Actions', label: 'Go to sessions', hint: '', run: go(href.sessions()) },
      { id: 'a:searchpage', group: 'Actions', label: 'Open search', hint: '', run: go(href.search()) },
      { id: 'a:settings', group: 'Actions', label: 'Settings', hint: '', run: go(href.settings()) },
      { id: 'a:theme', group: 'Actions', label: 'Toggle light / dark theme', hint: '', run: toggleTheme },
    ];
    const sessions: Item[] = app.sessions.map((s) => ({
      id: `s:${s.id}`,
      group: 'Sessions',
      label: sessionTitle(s),
      hint: `${agentLabel(s.agent)} · ${app.projectById.get(s.project_id)?.name ?? ''} · ${statusInfo(s.status).label}`,
      run: go(href.sessions(s.id)),
    }));
    const projects: Item[] = app.projects.map((p) => ({
      id: `p:${p.id}`,
      group: 'Projects',
      label: p.name,
      hint: p.paths[0]?.path ?? '',
      run: go(href.project(p.id)),
    }));
    const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
    const match = (it: Item): boolean => {
      const hay = `${it.label} ${it.hint}`.toLowerCase();
      return terms.every((t) => hay.includes(t));
    };
    const found = [...sessions.filter(match).slice(0, 30), ...projects.filter(match).slice(0, 20)];
    // With a query, jumping to a match is the common case; without one, actions lead.
    return q ? [...found, ...actions.filter(match)] : [...actions, ...found];
  });

  $effect(() => {
    if (active >= items.length) active = Math.max(0, items.length - 1);
  });

  function run(it: Item | undefined): void {
    if (!it) return;
    app.paletteOpen = false;
    it.run();
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const n = items.length;
      if (n) active = (active + (e.key === 'ArrowDown' ? 1 : -1) + n) % n;
      document.getElementById(`pal-${active}`)?.scrollIntoView({ block: 'nearest' });
    } else if (e.key === 'Enter') {
      e.preventDefault();
      run(items[active]);
    }
  }
</script>

<dialog
  bind:this={dialog}
  class="palette"
  aria-label="Command palette"
  onclose={() => (app.paletteOpen = false)}
  onclick={(e) => {
    if (e.target === dialog) app.paletteOpen = false;
  }}
>
  <input
    bind:this={input}
    bind:value={query}
    oninput={() => (active = 0)}
    onkeydown={onKey}
    class="q"
    type="text"
    placeholder="Jump to a session or project, or type a command…"
    role="combobox"
    aria-expanded="true"
    aria-controls="palette-list"
    aria-activedescendant={items.length ? `pal-${active}` : undefined}
    aria-autocomplete="list"
    autocomplete="off"
    spellcheck="false"
  />
  <ul id="palette-list" role="listbox" aria-label="Results">
    {#each items as it, i (it.id)}
      {#if i === 0 || items[i - 1]?.group !== it.group}
        <li class="group" role="presentation">{it.group}</li>
      {/if}
      <!-- Keyboard selection is handled on the combobox input (aria-activedescendant). -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <li
        id="pal-{i}"
        role="option"
        aria-selected={i === active}
        class:active={i === active}
        onclick={() => run(it)}
        onmousemove={() => (active = i)}
      >
        <span class="ellipsis">{it.label}</span>
        <span class="hint ellipsis">{it.hint}</span>
      </li>
    {:else}
      <li class="none" role="presentation">No matches</li>
    {/each}
  </ul>
</dialog>

<style>
  .palette {
    width: min(620px, calc(100vw - 24px));
    margin-top: 12vh;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 14px;
    background: var(--panel);
    color: var(--text);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
  }
  .palette::backdrop {
    background: rgb(15 17 21 / 0.35);
  }
  .q {
    width: 100%;
    border: 0;
    border-bottom: 1px solid var(--border);
    padding: 14px 16px;
    font-size: 15px;
    background: transparent;
    outline: none;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 6px;
    max-height: min(420px, 60vh);
    overflow: auto;
  }
  .group {
    padding: 8px 10px 4px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-3);
  }
  [role='option'] {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    padding: 8px 10px;
    border-radius: 8px;
    cursor: pointer;
  }
  [role='option'].active {
    background: var(--accent-soft);
  }
  [role='option'] .hint {
    max-width: 50%;
    flex: none;
  }
  .none {
    padding: 16px;
    color: var(--text-2);
    text-align: center;
  }
</style>
