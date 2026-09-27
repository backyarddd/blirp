<script lang="ts">
  import { app } from '../../lib/app.svelte';
  import { setTheme, theme, type ThemePref } from '../../lib/theme.svelte';
  import { NOTIFY_EVENTS, permissionHint, primeSound, type NotifyPrefs } from '../../lib/notify';

  const OPTIONS: { id: ThemePref; label: string }[] = [
    { id: 'system', label: 'Match system' },
    { id: 'light', label: 'Light' },
    { id: 'dark', label: 'Dark' },
  ];

  const prefs = $derived(app.notifyPrefs);
  const perm = $derived(app.notifyPermission);
  let test: { ok: boolean; detail: string } | null = $state(null);
  let testing = $state(false);

  function update(patch: Partial<NotifyPrefs>): void {
    app.setNotifyPrefs({ ...prefs, ...patch });
  }

  function setSound(on: boolean): void {
    if (on) primeSound(); // a click: lets later chimes play
    update({ sound: on });
  }

  async function sendTest(): Promise<void> {
    testing = true;
    test = null;
    try {
      test = await app.testNotification();
    } finally {
      testing = false;
    }
  }
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

<section class="card panel-pad" aria-labelledby="notify-h">
  <h2 class="h" id="notify-h">Notifications</h2>
  <label class="check">
    <input type="checkbox" checked={prefs.enabled} onchange={(e) => update({ enabled: e.currentTarget.checked })} />
    <span>Tell me when a session needs attention and I am not looking at it</span>
  </label>
  <fieldset class="events" disabled={!prefs.enabled}>
    <legend class="muted small">When a session</legend>
    {#each NOTIFY_EVENTS as ev (ev.id)}
      <label class="check">
        <input
          type="checkbox"
          checked={prefs.events[ev.id]}
          onchange={(e) => update({ events: { ...prefs.events, [ev.id]: e.currentTarget.checked } })}
        />
        <span>{ev.label}</span>
      </label>
    {/each}
    <label class="check">
      <input type="checkbox" checked={prefs.sound} onchange={(e) => setSound(e.currentTarget.checked)} />
      <span>Play a sound</span>
    </label>
  </fieldset>
  <p class="hint">
    In blirp: a toast when you are looking at another session, and a count in the window title while it is in the
    background. Outside blirp:
    {#if perm === 'desktop'}
      a system notification from the desktop app, and its taskbar button flashes (dock icon bounces on macOS).
    {:else if perm === 'granted'}
      a notification from this browser.
    {:else}
      {permissionHint(perm)}
    {/if}
  </p>
  <div class="row">
    {#if perm === 'default' || perm === 'denied'}
      <button type="button" class="btn" onclick={() => app.enableBrowserNotifications()}>Enable desktop notifications</button>
    {/if}
    <button type="button" class="btn" disabled={testing} onclick={sendTest}>Send test notification</button>
  </div>
  {#if test}
    <p class="hint" class:bad={!test.ok} role="status">
      {test.ok ? '' : 'Not shown:'}
      {test.detail}
      {#if test.ok}
        Nothing appeared? Check Do not disturb / Focus (Windows: Settings > System > Notifications, and allow blirp or
        your browser there; macOS: System Settings > Notifications).
      {/if}
    </p>
  {/if}
</section>

<style>
  .h {
    font-size: 15px;
    margin: 0 0 10px;
  }
  .events {
    border: 0;
    margin: 8px 0 0;
    padding: 0 0 0 24px;
    display: grid;
    gap: 4px;
  }
  .events legend {
    padding: 0;
    margin-bottom: 4px;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 10px;
  }
  .bad {
    color: var(--danger);
  }
</style>
