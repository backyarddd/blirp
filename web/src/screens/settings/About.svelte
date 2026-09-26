<script lang="ts">
  import { app } from '../../lib/app.svelte';
</script>

<section class="card panel-pad">
  <h2 class="h">About blirp</h2>
  <p>A workspace for CLI coding agents with automatic memory across sessions. Free and open source.</p>
  <dl class="facts">
    <dt>Daemon version</dt>
    <dd class="mono">{app.health?.version ?? 'unknown'}</dd>
    <dt>UI version</dt>
    <dd class="mono">{__APP_VERSION__}</dd>
    <dt>Machine</dt>
    <dd>{app.health ? `${app.health.machine.name} (${app.health.machine.os})` : 'unknown'}</dd>
    <dt>Sync role</dt>
    <dd>{app.health?.role ?? 'unknown'}</dd>
  </dl>
  <p class="hint">The desktop app checks for updates on launch. From a terminal, <code>blirp doctor</code> reports the health of every integration.</p>
</section>

<style>
  .h {
    font-size: 15px;
    margin: 0 0 8px;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 6px 16px;
    margin: 12px 0;
  }
  .facts dt {
    color: var(--text-2);
  }
  .facts dd {
    margin: 0;
  }
</style>
