<script lang="ts">
  import X from '@lucide/svelte/icons/x';
  import { app } from '../app.svelte';
  import { updates } from '../update.svelte';
  import { href } from '../router';

  $effect(() => updates.start());

  const status = $derived(updates.status);
</script>

{#if updates.phase !== 'idle'}
  <div class="update-banner" class:bad={updates.phase !== 'updating'} role="status" aria-live="polite" data-testid="update-banner">
    {#if updates.phase === 'updating'}
      <span>
        <strong>Updating blirp to {updates.target}…</strong> blirp restarts and this page reloads when it is back. In a browser, sign in again with
        <code>blirp open</code> if asked.
      </span>
    {:else if updates.phase === 'failed'}
      <span><strong>The update failed</strong>; blirp {status?.current} keeps running. {updates.failure}</span>
      <button type="button" class="icon-btn sm" aria-label="Close" title="Close" onclick={() => (updates.phase = 'idle')}><X size={14} /></button>
    {:else}
      <span>
        <strong>blirp did not come back.</strong> Run <code>blirp start</code> in a terminal; <code>blirp logs</code> and
        <code>logs/update.log</code> in the data folder say what happened.
      </span>
    {/if}
  </div>
{:else if updates.stale && !updates.staleDismissed}
  <div class="update-banner" role="status" data-testid="restart-banner">
    <span><strong>blirp was updated</strong> to {app.health?.version}.</span>
    {#if updates.stale === 'restart'}
      <button type="button" class="btn sm primary" onclick={() => updates.restartApp()}>Restart app</button>
    {:else}
      <button type="button" class="btn sm primary" onclick={() => location.reload()}>Reload</button>
    {/if}
    <span class="spacer"></span>
    <button type="button" class="icon-btn sm" aria-label="Dismiss" title="Dismiss" onclick={() => (updates.staleDismissed = true)}>
      <X size={14} />
    </button>
  </div>
{:else if updates.banner && status}
  <div class="update-banner" role="status" data-testid="update-banner">
    <span><strong>blirp {updates.banner} is available</strong> <span class="hide-sm">(this machine runs {status.current})</span></span>
    {#if status.self_update && app.local}
      <button type="button" class="btn sm primary" onclick={() => updates.updateNow()} disabled={updates.starting}
        >{updates.starting ? 'Starting…' : 'Update now'}</button>
      {#if status.notes_url}<a class="btn sm ghost" href={status.notes_url} target="_blank" rel="noreferrer">Release notes</a>{/if}
    {:else if !app.local}
      <!-- Portal devices and other machines' UIs cannot update this machine. -->
      <a class="btn sm ghost" href={href.settings('about')}>Details</a>
    {:else if status.notes_url}
      <a class="btn sm ghost" href={status.notes_url} target="_blank" rel="noreferrer">How to update</a>
    {/if}
    <span class="spacer"></span>
    <button type="button" class="icon-btn sm" aria-label="Dismiss" title="Hide until the next release" onclick={() => updates.dismiss()}>
      <X size={14} />
    </button>
  </div>
{/if}

<style>
  .update-banner {
    grid-row: 2;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px 10px;
    padding: 6px 16px;
    font-size: 13px;
    background: var(--accent-soft);
    border-bottom: 1px solid var(--border);
  }
  .update-banner.bad {
    background: var(--waiting-bg);
  }
  @media (max-width: 720px) {
    .update-banner {
      padding: 6px 10px;
    }
    .hide-sm {
      display: none;
    }
  }
</style>
