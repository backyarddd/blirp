<script lang="ts">
  import { onDestroy } from 'svelte';
  import CloudUpload from '@lucide/svelte/icons/cloud-upload';
  import { api } from '../api/client';
  import { app } from '../app.svelte';
  import { navigate } from '../router.svelte';
  import { href } from '../router';
  import { formatBytes } from '../files';

  // First run after upgrade or pairing: project files upload after a grace period, and this
  // says what will go where, with a way to review or turn it off first.
  let now = $state(Date.now());
  const timer = setInterval(() => (now = Date.now()), 15_000);
  onDestroy(() => clearInterval(timer));

  const f = $derived(app.files);
  const until = $derived(f?.grace_until ?? null);
  const show = $derived(f !== null && f.available && f.enabled && !f.paused && until !== null && until > now);
  const minutes = $derived(until === null ? 0 : Math.max(1, Math.ceil((until - now) / 60_000)));
  const text = $derived(
    f
      ? `Uploading ${f.folders} project ${f.folders === 1 ? 'folder' : 'folders'}${f.bytes > 0 ? ` (${formatBytes(f.bytes)})` : ''} ` +
          `to ${f.hub_name ?? 'the hub'} in ${minutes} min. Secrets, build output and ignored files stay here.`
      : '',
  );
  let busy = $state(false);

  async function startNow(): Promise<void> {
    busy = true;
    const o = await app.act(() => api.files.startNow(), 'Uploading project files now');
    busy = false;
    if (o) app.files = o;
  }

  async function turnOff(): Promise<void> {
    busy = true;
    const current = await app.act(() => api.settings.get());
    if (current) {
      const cfg = current.config;
      await app.saveSettings(
        { config: { ...cfg, sync: { ...cfg.sync, project_files: false } }, base: cfg },
        'Project file sync turned off on this machine',
      );
      await app.refreshFiles();
    }
    busy = false;
  }
</script>

{#if show && f}
  <div class="banner" role="region" aria-label="Project file sync">
    <CloudUpload size={16} aria-hidden="true" />
    <span class="text" data-testid="files-banner">{text}</span>
    <span class="actions">
      <button type="button" class="btn sm" onclick={() => navigate(href.projects())}>Review exclusions</button>
      {#if app.control}<button type="button" class="btn sm" onclick={startNow} disabled={busy}>Upload now</button>{/if}
      {#if app.admin}<button type="button" class="btn sm" onclick={turnOff} disabled={busy}>Turn off</button>{/if}
    </span>
  </div>
{/if}

<style>
  .banner {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border);
    background: var(--panel-2);
    font-size: 13px;
    min-width: 0;
  }
  .text {
    flex: 1 1 320px;
    min-width: 0;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
</style>
