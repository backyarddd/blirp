<script lang="ts">
  import { tick } from 'svelte';
  import Maximize from '@lucide/svelte/icons/maximize-2';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import GripVertical from '@lucide/svelte/icons/grip-vertical';
  import { sessionActions } from '../lib/actions';
  import { actionEnv, sessionOps } from '../lib/manage';
  import { contextmenu } from '../lib/contextmenu';
  import Menu from '../lib/components/Menu.svelte';
  import { app } from '../lib/app.svelte';
  import { href } from '../lib/router';
  import { agentLabel, sessionTitle } from '../lib/status';
  import StatusChip from '../lib/components/StatusChip.svelte';
  import Loadable from '../lib/components/Loadable.svelte';
  import Terminal from '../lib/terminal/Terminal.svelte';
  import MachineBadge from '../lib/components/MachineBadge.svelte';
  import { gridLayout, moveTile, orderTiles, readGridOrder, writeGridOrder } from '../lib/grid';

  // Browsers cap live WebGL contexts (~16); beyond a handful of tiles use the DOM renderer.
  const WEBGL_TILE_LIMIT = 6;
  const TILE_MIN_WIDTH = 460;
  const GAP = 12;

  let saved = $state(readGridOrder());
  const tiles = $derived(orderTiles(app.liveSessions, saved));
  let width = $state(0);
  // The page has GAP padding on both sides.
  const layout = $derived(gridLayout(tiles.length, Math.floor((width - GAP) / (TILE_MIN_WIDTH + GAP))));

  let dragging = $state<string | null>(null);
  let over = $state<string | null>(null);
  const TILE_MIME = 'application/x-blirp-tile';

  function move(id: string, to: number): void {
    const ids = tiles.map((t) => t.id);
    const from = ids.indexOf(id);
    if (from < 0 || from === to) return;
    saved = moveTile(ids, from, to);
    writeGridOrder(saved);
  }

  function onDragStart(e: DragEvent, id: string): void {
    if (!e.dataTransfer) return;
    e.dataTransfer.setData(TILE_MIME, id);
    e.dataTransfer.effectAllowed = 'move';
    const tile = (e.currentTarget as HTMLElement).closest('.tile');
    if (tile) e.dataTransfer.setDragImage(tile, 24, 16);
    dragging = id;
  }

  function onDragOver(e: DragEvent, id: string): void {
    if (dragging === null || !e.dataTransfer?.types.includes(TILE_MIME)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    over = id;
  }

  function onDrop(e: DragEvent, index: number): void {
    if (dragging === null || !e.dataTransfer?.types.includes(TILE_MIME)) return;
    // Also keeps the browser from dropping the id as text into the terminal's input.
    e.preventDefault();
    move(dragging, index);
    dragging = over = null;
  }

  async function onGripKey(e: KeyboardEvent, id: string, index: number): Promise<void> {
    const perRow = layout.columns / layout.span;
    const steps: Record<string, number> = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -perRow, ArrowDown: perRow };
    const step = steps[e.key];
    if (step === undefined) return;
    e.preventDefault();
    move(id, Math.max(0, index + step));
    // Moving the focused tile's element drops its focus; give it back so the keys keep working.
    await tick();
    document.querySelector<HTMLElement>(`[data-grip="${CSS.escape(id)}"]`)?.focus();
  }
</script>

<div class="grid-page" bind:clientWidth={width}>
  <Loadable
    loading={!app.sessionsLoaded}
    error={app.sessionsError}
    empty={tiles.length === 0}
    emptyText="No live terminals right now. Sessions you start from blirp appear here while they run."
    onretry={() => app.refreshSessions()}
  >
    {#snippet emptyAction()}
      {#if app.control}<button type="button" class="btn primary" onclick={() => app.openNewSession()}>New session</button>{/if}
    {/snippet}
    <div class="grid" style:grid-template-columns="repeat({layout.columns}, minmax(0, 1fr))">
      {#each tiles as s, i (s.id)}
        <section
          class="tile card"
          class:dragging={dragging === s.id}
          class:over={over === s.id && dragging !== s.id}
          aria-label={sessionTitle(s)}
          style:grid-column="span {i >= layout.lastFrom ? layout.lastSpan : layout.span}"
          ondragover={(e) => onDragOver(e, s.id)}
          ondrop={(e) => onDrop(e, i)}
        >
          <!-- The menu is on the header only: right-click in the terminal below belongs to xterm. -->
          <header
            use:contextmenu={{
              items: () => sessionActions(s, actionEnv(), sessionOps),
              label: sessionTitle(s),
              rename: app.control ? () => sessionOps.rename(s) : undefined,
            }}
          >
            <!-- A span, not a button: Firefox starts no drag from a <button>. -->
            <span
              role="button"
              tabindex="0"
              data-grip={s.id}
              class="grip icon-btn sm"
              draggable="true"
              aria-label="Move {sessionTitle(s)} (arrow keys)"
              title="Drag to move"
              ondragstart={(e) => onDragStart(e, s.id)}
              ondragend={() => (dragging = over = null)}
              onkeydown={(e) => void onGripKey(e, s.id, i)}><GripVertical size={14} /></span
            >
            <a class="name ellipsis" href={href.sessions(s.id)}>{sessionTitle(s)}</a>
            <span class="faint small ellipsis hide-sm">{app.projectLabel(s.project_id)} · {agentLabel(s.agent)}</span>
            <span class="spacer"></span>
            <MachineBadge machineId={s.machine_id} />
            <StatusChip session={s} />
            <Menu items={sessionActions(s, actionEnv(), sessionOps)} label="Actions for {sessionTitle(s)}" title="More actions" triggerClass="icon-btn sm" align="right"
              ><Ellipsis size={14} /></Menu
            >
            <a class="icon-btn sm" href={href.sessions(s.id)} aria-label="Open {sessionTitle(s)}" title="Open session"><Maximize size={14} /></a>
          </header>
          <div class="term">
            <Terminal sessionId={s.id} label="Terminal: {sessionTitle(s)}" webgl={tiles.length <= WEBGL_TILE_LIMIT} compact />
          </div>
        </section>
      {/each}
    </div>
  </Loadable>
</div>

<style>
  .grid-page {
    height: 100%;
    overflow: auto;
    padding: 12px;
  }
  .grid {
    display: grid;
    grid-auto-rows: minmax(300px, 1fr);
    gap: 12px;
    min-height: 100%;
  }
  .tile {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
    overflow: hidden;
  }
  .tile:focus-within {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .tile.dragging {
    opacity: 0.5;
  }
  .tile.over {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .grip {
    cursor: grab;
    margin-right: -4px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px 6px 4px;
    border-bottom: 1px solid var(--border);
    min-width: 0;
  }
  .name {
    font-weight: 600;
    text-decoration: none;
    flex: 0 1 auto;
  }
  .term {
    flex: 1;
    min-height: 0;
  }
  @media (max-width: 720px) {
    .hide-sm {
      display: none;
    }
  }
</style>
