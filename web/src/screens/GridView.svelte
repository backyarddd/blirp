<script lang="ts">
  import Maximize from '@lucide/svelte/icons/maximize-2';
  import { app } from '../lib/app.svelte';
  import { href } from '../lib/router';
  import { agentLabel, sessionTitle } from '../lib/status';
  import StatusChip from '../lib/components/StatusChip.svelte';
  import Loadable from '../lib/components/Loadable.svelte';
  import Terminal from '../lib/terminal/Terminal.svelte';

  // Browsers cap live WebGL contexts (~16); beyond a handful of tiles use the DOM renderer.
  const WEBGL_TILE_LIMIT = 6;
  const tiles = $derived(app.liveSessions);
</script>

<div class="grid-page">
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
    <div class="grid">
      {#each tiles as s (s.id)}
        <section class="tile card" aria-label={sessionTitle(s)}>
          <header>
            <a class="name ellipsis" href={href.sessions(s.id)}>{sessionTitle(s)}</a>
            <span class="faint small ellipsis hide-sm">{app.projectById.get(s.project_id)?.name ?? ''} · {agentLabel(s.agent)}</span>
            <span class="spacer"></span>
            <StatusChip session={s} />
            <a class="icon-btn sm" href={href.sessions(s.id)} aria-label="Open {sessionTitle(s)}" title="Open session"><Maximize size={14} /></a>
          </header>
          <div class="term">
            <Terminal sessionId={s.id} label="Terminal: {sessionTitle(s)}" webgl={tiles.length <= WEBGL_TILE_LIMIT} fontSize={12} />
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
    grid-template-columns: repeat(auto-fit, minmax(min(460px, 100%), 1fr));
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
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px 6px 12px;
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
