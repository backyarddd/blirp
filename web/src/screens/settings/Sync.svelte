<script lang="ts">
  import Copy from '@lucide/svelte/icons/copy';
  import { api, errorMessage } from '../../lib/api/client';
  import type { Device, Invite, Machine, Settings } from '../../lib/api/types';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { formatDateTime, formatRelative } from '../../lib/time';
  import Loadable from '../../lib/components/Loadable.svelte';
  import QrCode from '../../lib/components/QrCode.svelte';

  let { settings, onsaved }: { settings: Settings; onsaved: (s: Settings) => void } = $props();

  const status = new Resource(() => api.sync.status());
  const machines = new Resource(() => api.machines.list());
  const devices = new Resource(() => api.devices.list());

  $effect(() => {
    void status.load();
    void machines.load();
  });

  const role = $derived(status.data?.role ?? settings.sync.role);
  $effect(() => {
    if (role === 'hub') void devices.load();
  });

  let machineName = $derived(settings.machine.name);

  async function saveName(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const name = machineName.trim();
    if (!name) return;
    const s = await app.act(() => api.settings.patch({ machine: { name } }), 'Machine name saved');
    if (s) onsaved(s);
  }

  let enabling = $state(false);
  async function enableHub(): Promise<void> {
    if (!confirm('Make this machine the hub? Other machines will pair with it and replicate memory through it.')) return;
    enabling = true;
    const s = await app.act(() => api.sync.enableHub(), 'This machine is now the hub');
    enabling = false;
    if (s) {
      status.data = s;
      void app.refreshHealth();
    }
  }

  let invite: Invite | null = $state.raw(null);
  let inviting = $state(false);
  async function createInvite(): Promise<void> {
    inviting = true;
    invite = (await app.act(() => api.sync.invite())) ?? null;
    inviting = false;
  }

  let joinInvite = $state('');
  let joinCode = $state('');
  let joining = $state(false);
  let joinError: string | null = $state(null);
  async function join(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    joinError = null;
    const inv = joinInvite.trim();
    const code = joinCode.trim().toUpperCase();
    if (!/^[A-Z0-9]{4}-?[A-Z0-9]{4}$/.test(code)) {
      joinError = 'The pairing code looks like XXXX-XXXX.';
      return;
    }
    joining = true;
    try {
      status.data = await api.sync.join(inv, code);
      joinInvite = '';
      joinCode = '';
      app.toast('Paired with the hub. Memory will sync in the background.', 'info');
      void app.refreshHealth();
    } catch (err) {
      joinError = errorMessage(err);
    } finally {
      joining = false;
    }
  }

  async function copy(text: string, what: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      app.toast(`${what} copied`, 'info');
    } catch (e) {
      app.toast(`Copy failed: ${errorMessage(e)}`);
    }
  }

  async function revokeMachine(m: Machine): Promise<void> {
    if (!confirm(`Revoke ${m.name}? It will no longer be able to sync or be controlled.`)) return;
    const ok = await app.act(async () => {
      await api.machines.revoke(m.id);
      return true;
    }, `${m.name} revoked`);
    if (ok) void machines.reload();
  }

  async function revokeDevice(d: Device): Promise<void> {
    if (!confirm(`Revoke ${d.name}? It is signed out immediately.`)) return;
    const ok = await app.act(async () => {
      await api.devices.revoke(d.id);
      return true;
    }, `${d.name} revoked`);
    if (ok) void devices.reload();
  }

  async function setControl(d: Device, on: boolean): Promise<void> {
    const updated = await app.act(() => api.devices.setControl(d.id, on));
    if (updated) devices.data = (devices.data ?? []).map((x) => (x.id === updated.id ? updated : x));
    else void devices.reload();
  }
</script>

<section class="card panel-pad">
  <h2 class="h">This machine</h2>
  <form class="row wrap name" onsubmit={saveName}>
    <label class="field grow">
      <span>Machine name</span>
      <input class="input" bind:value={machineName} required />
    </label>
    <button type="submit" class="btn" disabled={machineName.trim() === settings.machine.name}>Save</button>
  </form>
  <Loadable loading={status.loading} error={status.error} empty={!status.data} onretry={() => status.load()}>
    {#if status.data}
      {@const s = status.data}
      <dl class="facts">
        <dt>Role</dt>
        <dd><span class="badge accent">{s.role}</span></dd>
        <dt>Machine id</dt>
        <dd class="mono small ellipsis" title={s.machine_id}>{s.machine_id}</dd>
        {#if s.role === 'node'}
          <dt>Hub</dt>
          <dd class="mono small ellipsis">{s.hub ?? 'unknown'}</dd>
          <dt>Connection</dt>
          <dd>
            <span class="dot" class:ok={s.connected} aria-hidden="true"></span>{s.connected ? 'Connected' : 'Offline, changes are queued'}
          </dd>
        {/if}
        {#if s.role !== 'standalone'}
          <dt>Last sync</dt>
          <dd>{s.last_sync_at ? formatRelative(s.last_sync_at) : 'Never'}</dd>
          <dt>Pending changes</dt>
          <dd>{s.pending_outbox}</dd>
        {/if}
      </dl>
    {/if}
  </Loadable>
</section>

{#if role === 'standalone'}
  <section class="card panel-pad">
    <h2 class="h">Sync across machines</h2>
    <p class="hint">
      One machine acts as the hub; the others pair with it. Memory, projects and session history replicate through the hub over an
      encrypted peer-to-peer connection. Nothing leaves your machines.
    </p>
    <div class="two">
      <div class="opt">
        <h3>Make this the hub</h3>
        <p class="small muted">Pick the machine that is on most often.</p>
        <button type="button" class="btn primary" onclick={enableHub} disabled={enabling}>{enabling ? 'Enabling…' : 'Enable hub'}</button>
      </div>
      <form class="opt" onsubmit={join}>
        <h3>Join a hub</h3>
        <label class="field">
          <span>Invite</span>
          <input class="input mono" bind:value={joinInvite} placeholder="blirp1-…" required spellcheck="false" autocomplete="off" />
        </label>
        <label class="field">
          <span>Pairing code</span>
          <input class="input mono code-in" bind:value={joinCode} placeholder="XXXX-XXXX" required spellcheck="false" autocomplete="one-time-code" />
        </label>
        {#if joinError}<p class="err" role="alert">{joinError}</p>{/if}
        <button type="submit" class="btn" disabled={joining}>{joining ? 'Pairing…' : 'Pair with hub'}</button>
      </form>
    </div>
  </section>
{/if}

{#if role === 'hub'}
  <section class="card panel-pad">
    <div class="row">
      <h2 class="h">Invite a machine</h2>
      <span class="spacer"></span>
      <button type="button" class="btn primary" onclick={createInvite} disabled={inviting}>{inviting ? 'Creating…' : invite ? 'New invite' : 'Create invite'}</button>
    </div>
    <p class="hint">On the other machine run <code>blirp pair &lt;invite&gt; &lt;code&gt;</code>, use Settings &gt; Machines &amp; Sync there, or scan the QR code with the desktop app. Codes are single use and expire after 10 minutes.</p>
    {#if invite}
      <div class="invite">
        <QrCode text={invite.uri} label="Pairing QR code" size={180} />
        <div class="inv-body">
          <span class="label">Pairing code</span>
          <div class="row">
            <span class="code mono">{invite.code}</span>
            <button type="button" class="icon-btn sm" aria-label="Copy pairing code" onclick={() => copy(invite?.code ?? '', 'Code')}><Copy size={14} /></button>
          </div>
          <label class="field">
            <span>Invite</span>
            <div class="row">
              <input class="input mono small" readonly value={invite.invite} onfocus={(e) => e.currentTarget.select()} />
              <button type="button" class="icon-btn" aria-label="Copy invite" onclick={() => copy(invite?.invite ?? '', 'Invite')}><Copy size={15} /></button>
            </div>
          </label>
          <span class="hint">Expires {formatDateTime(invite.expires_at)}</span>
        </div>
      </div>
    {/if}
  </section>

  <section class="card panel-pad">
    <h2 class="h">Devices</h2>
    <p class="hint">Paired machines and signed-in browsers. Terminal control lets a device type into live sessions.</p>
    <Loadable loading={devices.loading} error={devices.error} empty={(devices.data?.length ?? 0) === 0} emptyText="No paired devices yet." onretry={() => devices.load()}>
      <ul class="list">
        {#each devices.data ?? [] as d (d.id)}
          <li class="dev" class:revoked={d.revoked}>
            <div class="grow">
              <div class="row wrap">
                <strong>{d.name}</strong>
                <span class="badge">{d.kind}</span>
                {#if d.revoked}<span class="badge">revoked</span>{/if}
              </div>
              <span class="small muted">Paired {formatRelative(d.created_at)} · last seen {d.last_seen ? formatRelative(d.last_seen) : 'never'}</span>
            </div>
            {#if !d.revoked}
              <label class="check small">
                <input type="checkbox" checked={d.can_control_terminals} onchange={(e) => setControl(d, e.currentTarget.checked)} />
                Terminal control
              </label>
              <button type="button" class="btn sm danger" onclick={() => revokeDevice(d)}>Revoke</button>
            {/if}
          </li>
        {/each}
      </ul>
    </Loadable>
  </section>
{/if}

{#if role !== 'standalone'}
  <section class="card panel-pad">
    <h2 class="h">Machines</h2>
    <Loadable loading={machines.loading} error={machines.error} empty={(machines.data?.length ?? 0) === 0} emptyText="No other machines yet." onretry={() => machines.load()}>
      <ul class="list">
        {#each machines.data ?? [] as m (m.id)}
          <li class="dev" class:revoked={m.revoked}>
            <div class="grow">
              <div class="row wrap">
                <strong>{m.name}</strong>
                <span class="badge">{m.role}</span>
                <span class="small muted">{m.os}</span>
                {#if m.id === status.data?.machine_id}<span class="badge accent">this machine</span>{/if}
                {#if m.revoked}<span class="badge">revoked</span>{/if}
              </div>
              <span class="small muted">Last seen {m.last_seen ? formatRelative(m.last_seen) : 'never'}</span>
            </div>
            {#if role === 'hub' && !m.revoked && m.id !== status.data?.machine_id}
              <button type="button" class="btn sm danger" onclick={() => revokeMachine(m)}>Revoke</button>
            {/if}
          </li>
        {/each}
      </ul>
    </Loadable>
  </section>
{/if}

<style>
  .h {
    font-size: 15px;
    margin: 0 0 4px;
  }
  .name {
    align-items: flex-end;
    margin-top: 10px;
  }
  .name .btn {
    margin-bottom: 12px;
  }
  .grow {
    flex: 1;
    min-width: 0;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 6px 16px;
    margin: 0;
  }
  .facts dt {
    color: var(--text-2);
    font-size: 13px;
  }
  .facts dd {
    margin: 0;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--waiting);
  }
  .dot.ok {
    background: var(--completed);
  }
  .two {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    gap: 16px;
    margin-top: 12px;
  }
  .opt {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 14px;
  }
  .opt h3 {
    margin: 0 0 4px;
    font-size: 14px;
  }
  .code-in {
    text-transform: uppercase;
    max-width: 180px;
  }
  .invite {
    display: flex;
    gap: 20px;
    flex-wrap: wrap;
    align-items: flex-start;
    margin-top: 12px;
  }
  .inv-body {
    flex: 1;
    min-width: 240px;
    display: grid;
    gap: 6px;
  }
  .code {
    font-size: 26px;
    font-weight: 700;
    letter-spacing: 0.08em;
  }
  .dev {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    padding: 10px 0;
  }
  .dev.revoked {
    opacity: 0.6;
  }
  .err {
    color: var(--danger);
  }
</style>
