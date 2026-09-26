<script lang="ts">
  import { app } from '../../lib/app.svelte';
  import { setTheme, theme, type ThemePref } from '../../lib/theme.svelte';

  const OPTIONS: { id: ThemePref; label: string }[] = [
    { id: 'system', label: 'Match system' },
    { id: 'light', label: 'Light' },
    { id: 'dark', label: 'Dark' },
  ];

  const supported = typeof window !== 'undefined' && 'Notification' in window;
  const blocked = $derived(supported && app.notify === false && Notification.permission === 'denied');
</script>

<section class="card panel-pad">
  <h2 class="h">Theme</h2>
  <div class="pills" role="radiogroup" aria-label="Theme">
    {#each OPTIONS as o (o.id)}
      <button type="button" class="pill" role="radio" aria-checked={theme.pref === o.id} class:active={theme.pref === o.id} onclick={() => setTheme(o.id)}>
        {o.label}
      </button>
    {/each}
  </div>
  <p class="hint">Saved in this browser only.</p>
</section>

<section class="card panel-pad">
  <h2 class="h">Notifications</h2>
  <label class="check">
    <input type="checkbox" checked={app.notify} disabled={!supported} onchange={(e) => app.setNotify(e.currentTarget.checked)} />
    <span>Notify me when a session needs input or finishes while blirp is in the background</span>
  </label>
  {#if !supported}
    <p class="hint">This browser does not support notifications.</p>
  {:else if blocked}
    <p class="hint">Notifications are blocked for this site. Allow them in your browser's site settings, then turn this on.</p>
  {:else}
    <p class="hint">Your browser asks for permission the first time you turn this on.</p>
  {/if}
</section>

<style>
  .h {
    font-size: 15px;
    margin: 0 0 10px;
  }
</style>
