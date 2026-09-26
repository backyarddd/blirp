<script lang="ts">
  import { onMount } from 'svelte';
  import Copy from '@lucide/svelte/icons/copy';
  import { api, errorMessage } from '../../lib/api/client';
  import type { Device, Machine, SettingsView, SyncInvite } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { navigate } from '../../lib/router.svelte';
  import { href } from '../../lib/router';
  import { formatRelative } from '../../lib/time';
  import Loadable from '../../lib/components/Loadable.svelte';
  import QrCode from '../../lib/components/QrCode.svelte';
  import Expiry from '../../lib/components/Expiry.svelte';
  import Modal from '../../lib/components/Modal.svelte';
  import Portal from './Portal.svelte';

  let { settings, onsaved }: { settings: SettingsView; onsaved: (s: SettingsView) => void } = $props();

  const machines = new Resource(() => api.machines.list());
  const devices = new Resource(() => api.devices.list());

  const status = $derived(app.sync);
  const role = $derived(status?.role ?? settings.config.sync.role);

  // `sync_updated` (pairing, revocation, connection changes) refreshes the lists in place.
  let first = true;
  $effect(() => {
    void app.syncTick;
    if (first) {
      first = false;
      void app.refreshSync();
      void machines.load();
    } else {
      void machines.reload();
    }
  });
  $effect(() => {
    void app.syncTick;
    if (role === 'hub') void devices.reload();
  });

  let joinInvite = $state('');
  let joinCode = $state('');
  let deepLink = $state(false);

  // Desktop deep links land on /settings/sync?join=<invite>&code=<code>. Any web page can open such
  // a link, so it only prefills the form: pairing always goes through the confirmation below. The
  // query is dropped so the pairing code does not stay in the address bar or history.
  onMount(() => {
    const q = new URLSearchParams(location.search);
    const join = q.get('join');
    const code = q.get('code');
    if (join === null && code === null) return;
    joinInvite = join ?? '';
    joinCode = code ?? '';
    deepLink = true;
    navigate(href.settings('sync'), { replace: true });
  });

  let machineName = $derived(settings.config.machine.name);

  async function saveName(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const name = machineName.trim();
    if (!name) return;
    const s = await app.saveSettings(
      { config: { ...settings.config, machine: { ...settings.config.machine, name } }, base: settings.config },
      'Machine name saved',
    );
    if (s) onsaved(s);
  }

  let busy = $state(false);
  async function roleChange(fn: () => Promise<unknown>, success: string): Promise<void> {
    busy = true;
    const out = await app.act(fn, success);
    busy = false;
    if (out !== undefined) {
      await app.refreshSync();
      void app.refreshHealth();
      // The role change rewrote the config (role, hub, hub control).
      const s = await app.act(() => api.settings.get());
      if (s) onsaved(s);
    }
  }

  function enableHub(): void {
    if (!confirm('Make this machine the hub? Other machines pair with it and replicate memory through it.')) return;
    void roleChange(() => api.sync.enableHub(), 'This machine is now the hub');
  }

  function disableHub(): void {
    if (!confirm('Stop being the hub? Paired machines stop syncing until a hub is set up again.')) return;
    invite = null;
    void roleChange(() => api.sync.disableHub(), 'Hub disabled');
  }

  function leaveHub(): void {
    const hub = status?.hub;
    if (!hub || !confirm('Leave the hub? This machine stops syncing and becomes standalone.')) return;
    void roleChange(() => api.machines.revoke(hub).then(() => true), 'Left the hub');
  }

  let invite: SyncInvite | null = $state.raw(null);
  let inviting = $state(false);
  async function createInvite(): Promise<void> {
    inviting = true;
    invite = (await app.act(() => api.sync.invite())) ?? null;
    inviting = false;
  }

  // Codes use 23456789ABCDEFGHJKLMNPQRSTUVWXYZ (no 0/O/1/I); the hub gives the precise error.
  const CODE = /^[A-Z0-9]{4}-?[A-Z0-9]{4}$/;
  let joining = $state(false);
  let joinError: string | null = $state(null);

  /** The join the user is asked to confirm; `hubId` is null for a LAN join by code alone. */
  interface PendingJoin {
    invite: string;
    code: string;
    hubId: string | null;
  }
  let pending: PendingJoin | null = $state.raw(null);
  /** Unchecked by default: control is a separate, explicit decision (`sync.allow_hub_control`). */
  let allowControl = $state(false);

  const fingerprint = (id: string): string => id.match(/.{1,4}/g)?.join(' ') ?? id;

  // Step 1: read the hub's id from the invite (nothing is contacted) and ask for confirmation.
  async function join(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    joinError = null;
    const code = joinCode.trim().toUpperCase();
    if (!CODE.test(code)) {
      joinError = 'The pairing code looks like XXXX-XXXX.';
      return;
    }
    const invite = joinInvite.trim();
    joining = true;
    try {
      const preview = await api.sync.previewJoin(invite);
      allowControl = false;
      pending = { invite, code, hubId: preview.hub_id };
    } catch (err) {
      app.noteForbidden(err);
      joinError = errorMessage(err);
    } finally {
      joining = false;
    }
  }

  // Step 2: pair; the control choice is written together with the new role.
  async function confirmJoin(): Promise<void> {
    const j = pending;
    if (!j) return;
    joining = true;
    try {
      app.setSync(await api.sync.join({ invite: j.invite, code: j.code, allow_hub_control: allowControl }));
      pending = null;
      joinInvite = '';
      joinCode = '';
      deepLink = false;
      app.toast('Paired with the hub. Memory syncs in the background.', 'info');
      void app.refreshHealth();
      onsaved(await api.settings.get());
    } catch (err) {
      pending = null;
      app.noteForbidden(err);
      joinError = errorMessage(err);
    } finally {
      joining = false;
    }
  }

  // Node only: whether the hub (and machines paired with it) may control this machine.
  let savingControl = $state(false);
  async function setHubControl(on: boolean, input: HTMLInputElement): Promise<void> {
    const warning =
      'Allow the hub to control this machine? The hub and every machine paired with it could start agents and shells here and type into them, which means running any command as you.';
    if (on && !confirm(warning)) {
      input.checked = false;
      return;
    }
    savingControl = true;
    const s = await app.saveSettings(
      { config: { ...settings.config, sync: { ...settings.config.sync, allow_hub_control: on } }, base: settings.config },
      on ? 'The hub may now control this machine' : 'The hub can no longer control this machine',
    );
    savingControl = false;
    if (s) onsaved(s);
    else input.checked = settings.config.sync.allow_hub_control;
  }

  // mDNS on the local network (`sync.lan_discovery`); a running sync endpoint restarts to apply it.
  let savingDiscovery = $state(false);
  async function setLanDiscovery(on: boolean, input: HTMLInputElement): Promise<void> {
    savingDiscovery = true;
    const s = await app.saveSettings(
      { config: { ...settings.config, sync: { ...settings.config.sync, lan_discovery: on } }, base: settings.config },
      on ? 'Local network discovery on' : 'Local network discovery off',
    );
    savingDiscovery = false;
    if (s) onsaved(s);
    else input.checked = settings.config.sync.lan_discovery;
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
    if (!confirm(`Revoke ${m.name}? It can no longer sync or be controlled.`)) return;
    const ok = await app.act(() => api.machines.revoke(m.id).then(() => true), `${m.name} revoked`);
    if (ok) void machines.reload();
  }

  async function revokeDevice(d: Device): Promise<void> {
    if (!confirm(`Revoke ${d.name}? It is disconnected immediately.`)) return;
    const ok = await app.act(() => api.devices.revoke(d.id).then(() => true), `${d.name} revoked`);
    if (ok) void devices.reload();
  }

  async function setControl(d: Device, on: boolean): Promise<void> {
    const updated = await app.act(() => api.devices.patch(d.id, { can_control_terminals: on }));
    if (updated) devices.data = (devices.data ?? []).map((x) => (x.id === updated.id ? updated : x));
    else void devices.reload();
  }
</script>

<section class="card panel-pad">
  <h2 class="h">This machine</h2>
  <form class="row wrap name" onsubmit={saveName}>
    <label class="field grow">
      <span>Machine name</span>
      <input class="input" bind:value={machineName} required readonly={!app.admin} />
    </label>
    {#if app.admin}
      <button type="submit" class="btn" disabled={machineName.trim() === settings.config.machine.name}>Save</button>
    {/if}
  </form>
  {#if !status}
    <p class="muted small">Loading sync status…</p>
  {:else}
      <dl class="facts">
        <dt>Role</dt>
        <dd><span class="badge accent" data-testid="sync-role">{status.role}</span></dd>
        <dt>Machine id</dt>
        <dd class="mono small ellipsis" title={status.machine_id}>{status.machine_id}</dd>
        {#if status.role === 'node'}
          <dt>Hub</dt>
          <dd class="mono small ellipsis">{status.hub ?? 'unknown'}</dd>
        {/if}
        {#if status.role !== 'standalone'}
          <dt>Connection</dt>
          <dd>
            <span class="dot" class:ok={status.connected} aria-hidden="true"></span>
            {#if status.role === 'hub'}
              {status.connected ? 'Hub running' : 'Hub endpoint not running'}
            {:else}
              {status.connected ? 'Connected' : 'Offline, changes are queued'}
            {/if}
          </dd>
          <dt>Last sync</dt>
          <dd>{status.last_sync_at ? formatRelative(status.last_sync_at) : 'Never'}</dd>
        {/if}
        {#if status.role === 'node'}
          <dt>Pending changes</dt>
          <dd>{status.pending_outbox}</dd>
        {/if}
      </dl>
      {#if status.role === 'node'}
        <div class="gap">
          <label class="check">
            <input
              type="checkbox"
              checked={settings.config.sync.allow_hub_control}
              disabled={!app.admin || savingControl}
              onchange={(e) => setHubControl(e.currentTarget.checked, e.currentTarget)}
            />
            Allow the hub to control this machine
          </label>
          <p class="small muted">
            When on, the hub and machines paired with it can start and stop agents and shells here, type into terminals and
            change sessions and memory, so they can run any command as you. When off they can still read synced history,
            project files and live terminals, and edit shared memory.
          </p>
        </div>
      {/if}
      <div class="gap">
        <label class="check">
          <input
            type="checkbox"
            checked={settings.config.sync.lan_discovery}
            disabled={!app.admin || savingDiscovery}
            onchange={(e) => setLanDiscovery(e.currentTarget.checked, e.currentTarget)}
          />
          Find machines on the local network
        </label>
        <p class="small muted">
          Uses mDNS while this machine is a hub or node, so <code>blirp pair &lt;code&gt;</code> can find the hub without an
          invite. On macOS it needs the Local Network permission. When off, pairing needs the invite; paired machines still
          connect.
        </p>
      </div>
      {#if app.admin && status.role === 'hub'}
        <button type="button" class="btn sm danger gap" onclick={disableHub} disabled={busy}>Disable hub</button>
      {:else if app.admin && status.role === 'node'}
        <button type="button" class="btn sm danger gap" onclick={leaveHub} disabled={busy}>Leave hub</button>
      {/if}
  {/if}
  {#if !app.admin}
    <p class="small muted gap">Sync and devices can only be managed from this machine's own desktop app or CLI.</p>
  {/if}
</section>

{#if deepLink && role !== 'standalone'}
  <p class="card panel-pad warn" role="alert">
    You opened a pairing link, but this machine is already a {role}. Disable the hub or leave the current hub first.
  </p>
{/if}

{#if role === 'standalone' && app.admin}
  <section class="card panel-pad">
    <h2 class="h">Sync across machines</h2>
    <p class="hint">
      One machine acts as the hub; the others pair with it. Memory, projects and session history replicate through the hub over an
      encrypted peer-to-peer connection. Nothing goes through a blirp server.
    </p>
    <div class="two">
      <div class="opt">
        <h3>Make this the hub</h3>
        <p class="small muted">Pick the machine that is on most often.</p>
        <button type="button" class="btn primary" onclick={enableHub} disabled={busy}>{busy ? 'Working…' : 'Enable hub'}</button>
      </div>
      <form class="opt" onsubmit={join} aria-label="Join a hub">
        <h3>Join a hub</h3>
        <label class="field">
          <span>Invite <span class="faint">(optional on the same network)</span></span>
          <input class="input mono" bind:value={joinInvite} placeholder="blirp1-… or blirp://join/…" spellcheck="false" autocomplete="off" />
        </label>
        <label class="field">
          <span>Pairing code</span>
          <input class="input mono code-in" bind:value={joinCode} placeholder="XXXX-XXXX" required spellcheck="false" autocomplete="one-time-code" />
        </label>
        <p class="small muted">Leave the invite empty to find the hub on this local network by its code alone.</p>
        {#if joinError}<p class="err" role="alert">{joinError}</p>{/if}
        <button type="submit" class="btn" disabled={joining}>{joining ? 'Pairing…' : 'Pair with hub'}</button>
      </form>
    </div>
  </section>
{/if}

{#if role === 'hub' && app.admin}
  <section class="card panel-pad">
    <div class="row">
      <h2 class="h">Invite a machine</h2>
      <span class="spacer"></span>
      <button type="button" class="btn primary" onclick={createInvite} disabled={inviting}>{inviting ? 'Creating…' : invite ? 'New invite' : 'Create invite'}</button>
    </div>
    <p class="hint">
      On the other machine open Settings &gt; Machines &amp; Sync and enter the invite and code, scan the QR code with a phone that
      has the desktop app's link handler, or run <code>blirp pair &lt;invite&gt; &lt;code&gt;</code>. On the same network the code
      alone is enough. Invites are single use and expire after 10 minutes.
    </p>
    {#if invite}
      <div class="invite">
        <QrCode text={invite.uri} label="Pairing QR code" size={180} />
        <div class="inv-body">
          <span class="label">Pairing code</span>
          <div class="row">
            <span class="code mono" data-testid="pairing-code">{invite.code}</span>
            <button type="button" class="icon-btn sm" aria-label="Copy pairing code" onclick={() => copy(invite?.code ?? '', 'Code')}><Copy size={14} /></button>
          </div>
          <div class="field">
            <span>Invite</span>
            <div class="row">
              <input class="input mono small" aria-label="Invite" readonly value={invite.invite} onfocus={(e) => e.currentTarget.select()} />
              <button type="button" class="icon-btn" aria-label="Copy invite" onclick={() => copy(invite?.invite ?? '', 'Invite')}><Copy size={15} /></button>
            </div>
          </div>
          <div class="field">
            <span>Join link</span>
            <div class="row">
              <input class="input mono small" aria-label="Join link" readonly value={invite.uri} onfocus={(e) => e.currentTarget.select()} />
              <button type="button" class="icon-btn" aria-label="Copy join link" onclick={() => copy(invite?.uri ?? '', 'Join link')}><Copy size={15} /></button>
            </div>
          </div>
          <Expiry at={invite.expires_at} />
        </div>
      </div>
    {/if}
  </section>

  <section class="card panel-pad">
    <h2 class="h">Devices</h2>
    <p class="hint">Paired machines and signed-in browsers. Terminal control lets a device type into live sessions.</p>
    <Loadable loading={devices.loading && !devices.data} error={devices.error} empty={(devices.data?.length ?? 0) === 0} emptyText="No paired devices yet." onretry={() => devices.load()}>
      <ul class="list">
        {#each devices.data ?? [] as d (d.id)}
          <li class="dev" class:revoked={d.revoked}>
            <div class="grow">
              <div class="row wrap">
                <strong>{d.name}</strong>
                <span class="badge">{d.kind}</span>
                {#if d.revoked}<span class="badge">revoked</span>{/if}
              </div>
              <span class="small muted">Added {formatRelative(d.created_at)} · last seen {d.last_seen ? formatRelative(d.last_seen) : 'never'}</span>
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
    <Loadable loading={machines.loading && !machines.data} error={machines.error} empty={(machines.data?.length ?? 0) === 0} emptyText="No other machines yet." onretry={() => machines.load()}>
      <ul class="list">
        {#each machines.data ?? [] as m (m.id)}
          <li class="dev" class:revoked={m.revoked}>
            <div class="grow">
              <div class="row wrap">
                <strong>{m.name}</strong>
                <span class="badge">{m.role}</span>
                <span class="small muted">{m.os}</span>
                {#if m.id === status?.machine_id}<span class="badge accent">this machine</span>{/if}
                {#if m.revoked}<span class="badge">revoked</span>{/if}
              </div>
              <span class="small muted">Last seen {m.last_seen ? formatRelative(m.last_seen) : 'never'}</span>
            </div>
            {#if app.admin && role === 'hub' && !m.revoked && m.id !== status?.machine_id}
              <button type="button" class="btn sm danger" onclick={() => revokeMachine(m)}>Revoke</button>
            {/if}
          </li>
        {/each}
      </ul>
    </Loadable>
  </section>
{/if}

<Modal open={pending !== null} title="Pair with this hub?" onclose={() => (pending = null)}>
  {#if pending}
    {#if deepLink}
      <p class="warn" role="alert">
        This pairing link was opened from outside blirp (a web page, a QR code or another app). Only continue if you just
        created this invite on your own hub.
      </p>
    {/if}
    {#if pending.hubId}
      <p class="small muted">Hub machine id</p>
      <p class="mono fp" data-testid="hub-fingerprint">{fingerprint(pending.hubId)}</p>
      <p class="small">
        Compare it with the <strong>Machine id</strong> shown on the hub in Settings &gt; Machines &amp; Sync. Pairing checks
        that the hub holds this id's key. The hub's name is only exchanged once the code is verified.
      </p>
    {:else}
      <p class="warn">
        No invite: the hub is found on this network by its pairing code, so its id cannot be shown in advance. Only continue
        if you just created this code on your own hub.
      </p>
    {/if}
    <p class="small"><strong>Once paired, the hub</strong> and every machine paired with it:</p>
    <ul class="small rights">
      <li>receive this machine's synced history: projects and folder paths, redacted session transcripts, summaries and memory;</li>
      <li>can read project files, git status and diffs here, and watch live terminals;</li>
      <li>can edit shared memory (brief, records, wiki, resources), which agents you start here are given.</li>
    </ul>
    <label class="check">
      <input type="checkbox" bind:checked={allowControl} />
      Allow this hub to start and control terminals on this machine
    </label>
    <p class="small muted">
      Also lets them stop and resume sessions, type into terminals and change sessions and memory here, which means running
      any command as you. You can change this later in Settings &gt; Machines &amp; Sync.
    </p>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (pending = null)}>Cancel</button>
    <button type="button" class="btn primary" onclick={confirmJoin} disabled={joining}>{joining ? 'Pairing…' : 'Pair with hub'}</button>
  {/snippet}
</Modal>

{#if role === 'hub' || settings.config.portal.lan}
  <Portal {settings} {onsaved} />
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
  .gap {
    margin-top: 10px;
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
  .warn {
    color: var(--waiting);
    margin: 0;
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
  .fp {
    font-size: 15px;
    letter-spacing: 0.04em;
    word-break: break-all;
    margin: 2px 0 8px;
  }
  .rights {
    margin: 4px 0 12px;
    padding-left: 18px;
  }
</style>
