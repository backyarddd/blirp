<script lang="ts">
  import { untrack } from 'svelte';
  import { api } from '../../lib/api/client';
  import type { BrowserInvite, PortalConfig, SettingsView } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import QrCode from '../../lib/components/QrCode.svelte';
  import Expiry from '../../lib/components/Expiry.svelte';

  let { settings, onsaved }: { settings: SettingsView; onsaved: (s: SettingsView) => void } = $props();

  const status = $derived(app.sync);
  const running = $derived(status?.portal_url != null);

  let port = $state(untrack(() => settings.config.portal.lan_port));
  let saving = $state(false);

  async function patchPortal(p: Partial<PortalConfig>): Promise<void> {
    saving = true;
    const config = { ...settings.config, portal: { ...settings.config.portal, ...p } };
    const s = await app.saveSettings({ config }, 'Portal settings saved');
    saving = false;
    if (s) {
      onsaved(s);
      port = s.config.portal.lan_port;
      void app.refreshSync();
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
    A hub can serve this UI over HTTPS on your local network so a phone or another computer can watch and steer sessions. Devices
    sign in by scanning a one-time QR code from a screen that is already signed in. Prefer <code>tailscale serve</code> for access
    from outside your network.
  </p>
  {#if app.admin}
    <label class="check">
      <input type="checkbox" checked={settings.config.portal.lan} disabled={saving} onchange={(e) => patchPortal({ lan: e.currentTarget.checked })} />
      <span>Serve the portal on the local network</span>
    </label>
    <form class="row wrap port" onsubmit={savePort}>
      <label class="field">
        <span>HTTPS port</span>
        <input class="input" type="number" min="1024" max="65535" bind:value={port} />
      </label>
      <button type="submit" class="btn" disabled={saving || port === settings.config.portal.lan_port}>Save port</button>
    </form>
    <p class="hint">The portal runs while this machine is the hub. Changes apply right away; if the port is taken the setting is kept and the portal stays off until you pick a free one.</p>
  {/if}
  {#if running && status}
    <dl class="facts">
      <dt>Address</dt>
      <dd class="mono small" data-testid="portal-url">{status.portal_url}</dd>
      <dt>Certificate fingerprint</dt>
      <dd class="mono small fp">{status.portal_cert_fingerprint ?? 'Not generated yet'}</dd>
    </dl>
    <p class="hint">The certificate is self-signed. Check the fingerprint when your browser asks you to trust it.</p>
  {:else if settings.config.portal.lan}
    <p class="small muted">The portal is not running.</p>
  {/if}
</section>

{#if running && app.admin}
  <section class="card panel-pad">
    <div class="row">
      <h2 class="h">Sign in a phone or browser</h2>
      <span class="spacer"></span>
      <button type="button" class="btn primary" onclick={createLogin}>{login ? 'New code' : 'Show login QR'}</button>
    </div>
    <p class="hint">The code works once and expires after 5 minutes. New devices start view-only; allow terminal control under Devices.</p>
    {#if login}
      <div class="row wrap qr">
        <QrCode text={login.url} label="Portal login QR code" size={200} />
        <div class="small">
          <p class="mono url">{login.url}</p>
          <Expiry at={login.expires_at} />
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
