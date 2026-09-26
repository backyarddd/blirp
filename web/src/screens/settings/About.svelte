<script lang="ts">
  import { api } from '../../lib/api/client';
  import { app } from '../../lib/app.svelte';

  let stopping = $state(false);

  async function shutdown(): Promise<void> {
    if (!confirm('Stop the blirp daemon? Every running session on this machine ends, and this page disconnects.')) return;
    stopping = true;
    const ok = await app.act(() => api.shutdown().then(() => true));
    if (!ok) {
      stopping = false;
      return;
    }
    app.stopStream();
    app.auth = 'offline';
    app.bootError = 'The daemon was stopped. Start blirp again to reconnect.';
  }
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

{#if app.admin}
  <section class="card panel-pad">
    <h2 class="h">Daemon</h2>
    <p class="hint">The daemon keeps sessions and sync running when no window is open. Stopping it ends all running sessions on this machine.</p>
    <button type="button" class="btn danger" onclick={shutdown} disabled={stopping}>{stopping ? 'Stopping…' : 'Stop daemon'}</button>
  </section>
{/if}

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
