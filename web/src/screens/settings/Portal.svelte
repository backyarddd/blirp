<script lang="ts">
  import { untrack } from 'svelte';
  import { api } from '../../lib/api/client';
  import type { BrowserInvite, Settings } from '../../lib/api/types';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { formatDateTime } from '../../lib/time';
  import QrCode from '../../lib/components/QrCode.svelte';

  let { settings, onsaved }: { settings: Settings; onsaved: (s: Settings) => void } = $props();

  const status = new Resource(() => api.sync.status());
  $effect(() => {
    void status.load();
  });

  let port = $state(untrack(() => settings.portal.lan_port));
  let saving = $state(false);

  async function patchPortal(p: Partial<Settings['portal']>): Promise<void> {
    saving = true;
    const s = await app.act(() => api.settings.patch({ portal: p }), 'Portal settings saved');
    saving = false;
    if (s) {
      onsaved(s);
      void status.reload();
    }
  }

  async function savePort(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (!Number.isInteger(port) || port < 1024 || port > 65535) {
      app.toast('Pick a port between 1024 and 65535.');
      return;
    }
    await patchPortal({ lan_port: port });
  }

  let login: BrowserInvite | null = $state.raw(null);
  async function createLogin(): Promise<void> {
    login = (await app.act(() => api.devices.browserInvite())) ?? null;
  }
</script>

<section class="card panel-pad">
  <h2 class="h">LAN portal</h2>
  <p class="hint">
    Serve this UI over HTTPS on your local network so a phone or another computer can watch and steer sessions. Devices sign in
    by scanning a one-time QR code from a screen that is already signed in. Prefer <code>tailscale serve</code> for access from
    outside your network.
  </p>
  <label class="check">
    <input type="checkbox" checked={settings.portal.lan} disabled={saving} onchange={(e) => patchPortal({ lan: e.currentTarget.checked })} />
    <span>Serve the portal on the local network</span>
  </label>
  <form class="row wrap port" onsubmit={savePort}>
    <label class="field">
      <span>HTTPS port</span>
      <input class="input" type="number" min="1024" max="65535" bind:value={port} />
    </label>
    <button type="submit" class="btn" disabled={saving || port === settings.portal.lan_port}>Save port</button>
  </form>
  {#if settings.portal.lan && status.data}
    <dl class="facts">
      <dt>Address</dt>
      <dd class="mono small">{status.data.portal_url ?? 'Starting…'}</dd>
      <dt>Certificate fingerprint</dt>
      <dd class="mono small fp">{status.data.portal_cert_fingerprint ?? 'Not generated yet'}</dd>
    </dl>
    <p class="hint">The certificate is self-signed. Check the fingerprint when your browser asks you to trust it.</p>
  {/if}
</section>

{#if settings.portal.lan}
  <section class="card panel-pad">
    <div class="row">
      <h2 class="h">Sign in a phone or browser</h2>
      <span class="spacer"></span>
      <button type="button" class="btn primary" onclick={createLogin}>{login ? 'New code' : 'Show login QR'}</button>
    </div>
    <p class="hint">The code works once and expires after 5 minutes. Manage signed-in devices under Machines &amp; Sync.</p>
    {#if login}
      <div class="row wrap qr">
        <QrCode text={login.url} label="Portal login QR code" size={200} />
        <div class="small">
          <p class="mono url">{login.url}</p>
          <p class="muted">Expires {formatDateTime(login.expires_at)}</p>
        </div>
      </div>
    {/if}
  </section>
{/if}

<style>
  .h {
    font-size: 15px;
    margin: 0 0 4px;
  }
  .port {
    align-items: flex-end;
    margin-top: 12px;
  }
  .port .btn {
    margin-bottom: 12px;
  }
  .port .field {
    width: 160px;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 6px 16px;
    margin: 4px 0 8px;
  }
  .facts dt {
    color: var(--text-2);
    font-size: 13px;
  }
  .facts dd {
    margin: 0;
  }
  .fp {
    overflow-wrap: anywhere;
  }
  .qr {
    align-items: flex-start;
    gap: 20px;
    margin-top: 8px;
  }
  .url {
    overflow-wrap: anywhere;
    max-width: 360px;
  }
</style>
