<script lang="ts">
  import { api, errorMessage } from '../../lib/api/client';
  import type { UpdateStatus } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';

  let stopping = $state(false);
  let update: UpdateStatus | null = $state.raw(null);
  let updateError: string | null = $state(null);

  $effect(() => {
    api.update().then(
      (u) => (update = u),
      (e: unknown) => (updateError = errorMessage(e)),
    );
  });

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
  <p class="hint">From a terminal, <code>blirp doctor</code> reports the health of every integration.</p>
</section>

<section class="card panel-pad" aria-live="polite">
  <h2 class="h">Updates</h2>
  {#if updateError}
    <p class="hint">Could not ask the daemon about updates: {updateError}</p>
  {:else if !update}
    <p class="hint">Checking for updates…</p>
  {:else if !update.enabled}
    <p class="hint">Update checks are off (<code>[update] check = false</code> in config.toml).</p>
  {:else if update.available}
    <p>
      <strong>blirp {update.latest} is available</strong> (this machine runs {update.current}).
      {#if update.notes_url}<a href={update.notes_url} target="_blank" rel="noreferrer">Release notes</a>{/if}
    </p>
    {#if update.self_update}
      <p class="hint">
        Run <code>blirp update</code> in a terminal. It stops the daemon (running sessions end), replaces blirp and the desktop app, and starts
        the daemon again.
      </p>
    {:else}
      <p class="hint">
        This blirp was not installed by the install script (it came from an installer, a package manager or a source build), so
        <code>blirp update</code> does not replace it. Update it the way you installed it: for example <code>brew upgrade blirp</code>, your
        Linux package manager, or the installer from the
        {#if update.notes_url}<a href={update.notes_url} target="_blank" rel="noreferrer">release page</a>{:else}release page{/if}.
      </p>
    {/if}
  {:else if update.latest}
    <p class="hint">blirp {update.current} is up to date.</p>
  {:else}
    <p class="hint">Could not reach GitHub to check for updates; the daemon tries again within an hour.</p>
  {/if}
</section>

{#if app.local}
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
