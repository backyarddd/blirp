<script lang="ts">
  import { untrack } from 'svelte';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
  import { api } from '../../lib/api/client';
  import type {
    AppliedFiles,
    FilesIncoming,
    FilesMode,
    FilesPreview,
    FilesRoot,
    ProjectSummary,
  } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { formatRelative } from '../../lib/time';
  import { REASON_LABELS, STATE_LABELS, formatBytes } from '../../lib/files';
  import Loadable from '../../lib/components/Loadable.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const files = new Resource(() => api.files.project(pid));
  let loaded = false;
  $effect(() => {
    // First load, then quiet refreshes whenever file sync state changes.
    void app.filesTick;
    void pid;
    untrack(() => {
      void (loaded ? files.reload() : files.load());
      loaded = true;
    });
  });

  const MODES: { id: FilesMode; label: string }[] = [
    { id: 'default', label: 'Default' },
    { id: 'on', label: 'On' },
    { id: 'off', label: 'Off' },
  ];
  let savingMode = $state(false);
  async function setMode(mode: FilesMode): Promise<void> {
    savingMode = true;
    const f = await app.act(() => api.files.setMode(pid, mode), `File sync for ${project.name}: ${mode}`);
    savingMode = false;
    if (f) files.data = f;
  }

  let previewFor: string | null = $state(null);
  let preview: FilesPreview | null = $state.raw(null);
  let previewError: string | null = $state(null);
  let previewing = $state(false);
  async function showPreview(path: string): Promise<void> {
    if (previewFor === path) {
      previewFor = null;
      return;
    }
    previewFor = path;
    preview = null;
    previewError = null;
    previewing = true;
    try {
      preview = await api.files.preview(pid, { root: path });
    } catch (e) {
      previewError = e instanceof Error ? e.message : String(e);
    } finally {
      previewing = false;
    }
  }

  let incomingFor: string | null = $state(null);
  let incoming: FilesIncoming | null = $state.raw(null);
  let incomingError: string | null = $state(null);
  async function checkIncoming(path: string): Promise<void> {
    incomingFor = path;
    incoming = null;
    incomingError = null;
    try {
      incoming = await api.files.incoming(pid, path);
    } catch (e) {
      incomingError = e instanceof Error ? e.message : String(e);
    }
  }

  let applying = $state(false);
  let applied: AppliedFiles | null = $state.raw(null);
  async function applyHub(r: FilesRoot, origin: boolean): Promise<void> {
    const path = r.local?.path;
    if (!path) return;
    applying = true;
    applied =
      (await app.act(() => api.files.apply(pid, path), origin ? "Brought the hub's changes here" : 'Updated from the hub')) ?? null;
    applying = false;
    incomingFor = null;
    void files.reload();
  }

  async function deleteHubCopy(r: FilesRoot): Promise<void> {
    const msg =
      `Delete the hub copy of ${r.path} (${r.machine_name})? Its files and history on the hub are removed. ` +
      'Folders on your machines stay as they are; copies elsewhere stop syncing.';
    if (!confirm(msg)) return;
    const ok = await app.act(() => api.files.deleteRoot(pid, r.root_id).then(() => true), 'Hub copy deleted');
    if (ok) void files.reload();
  }

  const ACTIONS: Record<string, string> = {
    update: 'updated',
    new: 'new',
    delete: 'deleted',
    conflict: 'kept next to your change',
    skip: 'cannot be held here',
  };
</script>

<Loadable loading={files.loading} error={files.error} empty={!files.data} onretry={() => files.load()}>
  {#if files.data}
    {@const f = files.data}
    <section class="card panel-pad">
      <div class="row wrap head">
        <h2 class="h">Files on hub</h2>
        <span class="spacer"></span>
        <button type="button" class="btn sm" onclick={() => files.reload()} disabled={files.loading}
          ><RefreshCw size={14} aria-hidden="true" />Refresh</button
        >
      </div>
      <p class="hint">
        Uploads this project's folders to your hub, so cloud sessions and your other machines can work on them, including
        uncommitted changes. Secrets, build output and files your <code>.gitignore</code> or <code>.blirpignore</code> leaves out
        stay on the machine. Nothing is written into your folders unless you ask.
      </p>
      {#if !f.available}
        <p class="notice">File sync needs a hub: make this machine the hub or pair it with one (Settings &gt; Machines &amp; Sync).</p>
      {:else}
        {#if f.hub_error}<p class="notice warn" role="alert"><TriangleAlert size={14} aria-hidden="true" /> {f.hub_error}</p>{/if}
        <div class="field">
          <span id="files-mode">Upload this project's files</span>
          <div class="pills" role="radiogroup" aria-labelledby="files-mode" data-testid="files-mode">
            {#each MODES as m (m.id)}
              <button
                type="button"
                class="pill"
                role="radio"
                aria-checked={f.mode === m.id}
                class:active={f.mode === m.id}
                disabled={!app.control || savingMode}
                onclick={() => setMode(m.id)}>{m.label}</button
              >
            {/each}
          </div>
          <span class="hint" data-testid="files-effective">
            {#if f.mode === 'default'}
              Follows each machine's setting; here file sync is {f.global ? 'on' : 'off'}.
            {:else if f.mode === 'on'}
              Uploads on every machine.
            {:else}
              Uploads stopped; the hub copy stays readable.
            {/if}
            {#if f.paused}File sync is paused on this machine.{/if}
          </span>
        </div>
      {/if}
    </section>

    {#if f.available}
      {#if applied}
        <p class="notice" role="status">
          {applied.written} written, {applied.deleted} removed{applied.conflicts.length ? `, ${applied.conflicts.length} kept as conflict copies next to your changes` : ''}.
          {#if applied.skipped.length}{applied.skipped.length} skipped.{/if}
          {#if applied.failed.length}<span class="error">{applied.failed.length} failed: {applied.failed.join('; ')}</span>{/if}
        </p>
      {/if}
      {#if f.roots.length === 0}
        <p class="muted">No folder of this project syncs yet.</p>
      {/if}
      <ul class="roots">
        {#each f.roots as r (r.root_id)}
          {@const local = r.local}
          {@const origin = local?.origin === true}
          <li class="card panel-pad root" data-testid="files-root">
            <div class="row wrap">
              <strong>{r.machine_name || 'unknown machine'}</strong>
              {#if local}<span class="badge accent">{origin ? 'this machine' : 'copy on this machine'}</span>{/if}
              {#if r.origin_revoked}<span class="badge">from a revoked machine</span>{/if}
              {#if (r.hub?.conflicts ?? 0) > 0}
                <span class="badge conflict" data-testid="conflict-badge" title="Files kept as name.conflict-machine-time.ext after concurrent edits"
                  >{r.hub?.conflicts} conflict {r.hub?.conflicts === 1 ? 'copy' : 'copies'}</span
                >
              {/if}
            </div>
            <div class="mono small ellipsis muted" title={r.path}>{r.path}</div>
            {#if local && local.path !== r.path}<div class="mono small ellipsis muted" title={local.path}>here: {local.path}</div>{/if}
            <dl class="facts small">
              <dt>On hub</dt>
              <dd>
                {#if r.hub}
                  {r.hub.files} files · {formatBytes(r.hub.bytes)} · updated {formatRelative(r.hub.updated_at)}
                {:else}
                  not uploaded yet
                {/if}
              </dd>
              {#if local}
                <dt>Here</dt>
                <dd>
                  <span class="state {local.state}" data-testid="files-state">{STATE_LABELS[local.state]}</span>
                  {#if local.message}<span class="muted"> · {local.message}</span>{/if}
                  {#if local.last_upload_at} · last upload {formatRelative(local.last_upload_at)}{/if}
                  {#if local.pending > 0} · {local.pending} pending{/if}
                </dd>
                {#if local.excluded.length}
                  <dt>Left out</dt>
                  <dd>{local.excluded.map((g) => `${g.count} ${REASON_LABELS[g.reason].toLowerCase()}`).join(' · ')}</dd>
                {/if}
                {#if local.reincluded_secrets.length}
                  <dt>Secrets uploaded</dt>
                  <dd class="error">{local.reincluded_secrets.join(', ')} (re-included by .blirpignore)</dd>
                {/if}
              {/if}
            </dl>
            <div class="row wrap actions">
              {#if origin && local}
                <button type="button" class="btn sm" onclick={() => showPreview(local.path)} aria-expanded={previewFor === local.path}
                  >{previewFor === local.path ? 'Hide preview' : 'Preview'}</button
                >
              {/if}
              {#if local && r.hub && app.control}
                <button type="button" class="btn sm" onclick={() => checkIncoming(local.path)}>Check the hub for changes</button>
              {/if}
              {#if r.hub && app.control && (f.mode === 'off' || r.origin_revoked)}
                <button type="button" class="btn sm danger" onclick={() => deleteHubCopy(r)}>Delete hub copy</button>
              {/if}
            </div>

            {#if local && incomingFor === local.path}
              <div class="panel" aria-live="polite">
                {#if incomingError}
                  <p class="error">{incomingError}</p>
                {:else if !incoming}
                  <p class="muted">Asking the hub…</p>
                {:else if incoming.files.length === 0}
                  <p class="muted" data-testid="files-incoming">Up to date with the hub.</p>
                {:else}
                  <p data-testid="files-incoming">Hub has {incoming.files.length} newer {incoming.files.length === 1 ? 'file' : 'files'}:</p>
                  <ul class="paths mono small">
                    {#each incoming.files.slice(0, 50) as i (i.path)}
                      <li>{i.path} <span class="muted">({ACTIONS[i.action]}{i.by_machine_name ? `, ${i.by_machine_name}` : ''})</span></li>
                    {/each}
                  </ul>
                  <button type="button" class="btn sm primary" disabled={applying} onclick={() => applyHub(r, origin)}
                    >{applying ? 'Working…' : origin ? 'Bring changes here' : 'Update from hub'}</button
                  >
                  <p class="small muted">Files you changed here are kept; the hub's version is saved next to them as a conflict copy.</p>
                {/if}
              </div>
            {/if}

            {#if local && previewFor === local.path}
              <div class="panel" data-testid="files-preview">
                {#if previewing}
                  <p class="muted" role="status">Scanning the folder…</p>
                {:else if previewError}
                  <p class="error">{previewError}</p>
                {:else if preview}
                  {#if preview.never_synced}<p class="notice warn">{preview.never_synced}</p>{/if}
                  {#if preview.state === 'too_large'}
                    <p class="notice warn">Too large to sync. Add a <code>.blirpignore</code> to leave big folders out.</p>
                  {/if}
                  <p><strong>{preview.files}</strong> files, {formatBytes(preview.bytes)} would upload.</p>
                  {#if preview.reincluded_secrets.length}
                    <p class="error">Uploaded although they look like secrets (re-included by .blirpignore): {preview.reincluded_secrets.join(', ')}</p>
                  {/if}
                  {#each preview.excluded as g (g.reason)}
                    <details class="group">
                      <summary>{REASON_LABELS[g.reason]}: {g.count}</summary>
                      <ul class="paths mono small">
                        {#each g.paths as p (p)}<li>{p}</li>{/each}
                        {#if g.count > g.paths.length}<li class="muted">and {g.count - g.paths.length} more</li>{/if}
                      </ul>
                    </details>
                  {/each}
                {/if}
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</Loadable>

<style>
  .head {
    margin-bottom: 4px;
  }
  .head .h {
    margin: 0;
  }
  .roots {
    list-style: none;
    padding: 0;
    margin: 12px 0 0;
    display: grid;
    gap: 12px;
  }
  .root {
    display: grid;
    gap: 6px;
    min-width: 0;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 2px 12px;
    margin: 4px 0;
  }
  .facts dt {
    color: var(--text-3);
  }
  .facts dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .actions {
    gap: 8px;
  }
  .panel {
    border-top: 1px solid var(--border);
    padding-top: 8px;
    display: grid;
    gap: 6px;
  }
  .panel p {
    margin: 0;
  }
  .paths {
    list-style: none;
    padding: 0;
    margin: 4px 0;
    max-height: 240px;
    overflow: auto;
    overflow-wrap: anywhere;
  }
  .group summary {
    cursor: pointer;
  }
  .notice {
    display: flex;
    gap: 6px;
    align-items: center;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel-2);
    margin: 8px 0 0;
  }
  .notice.warn {
    border-color: var(--waiting);
    color: var(--waiting);
  }
  .error {
    color: var(--danger);
  }
  .badge.conflict {
    color: var(--waiting);
    border-color: var(--waiting);
  }
  .state.error,
  .state.too_large {
    color: var(--danger);
  }
  .state.idle {
    color: var(--completed);
  }
</style>
