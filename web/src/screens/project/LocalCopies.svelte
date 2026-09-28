<script lang="ts">
  // This machine's downloaded copies of the project's folders (file sync through the hub): detach one
  // so it stops syncing, or forget it so blirp stops tracking it. Neither touches files on disk.
  import Unlink from '@lucide/svelte/icons/unlink';
  import EyeOff from '@lucide/svelte/icons/eye-off';
  import { api } from '../../lib/api/client';
  import type { LocalCopy, LocalCopyMode, ProjectSummary } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { dialogs } from '../../lib/dialogs.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { formatRelative } from '../../lib/time';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const copies = new Resource(() => api.files.copies(pid));
  $effect(() => {
    void app.filesTick;
    void copies.reload();
  });

  const MODE: Record<LocalCopyMode, string> = {
    syncing: 'syncing',
    pending: 'download not finished',
    detached: 'detached',
  };

  let busy = $state('');

  async function detach(c: LocalCopy): Promise<void> {
    const ok = await dialogs.confirm({
      title: 'Detach copy?',
      body:
        `${c.path} stops syncing for good: it no longer uploads your edits or takes the hub's changes. Its files stay ` +
        'where they are and it stays a folder of this project. To sync again, download a new copy.',
      confirm: 'Detach',
      danger: true,
    });
    if (!ok) return;
    busy = c.path;
    const out = await app.act(() => api.files.detachCopy(pid, c.path), 'Copy detached; its files stay on disk');
    busy = '';
    if (out) copies.data = (copies.data ?? []).map((x) => (x.path === out.path ? out : x));
  }

  async function forget(c: LocalCopy): Promise<void> {
    const ok = await dialogs.confirm({
      title: 'Forget copy?',
      body:
        `blirp stops tracking ${c.path}` +
        (c.registered ? ' and removes it from this project on this machine' : '') +
        '. Nothing is deleted: the folder and all its files stay on disk.',
      confirm: 'Forget copy',
      danger: true,
    });
    if (!ok) return;
    busy = c.path;
    const done = await app.act(async () => (await api.files.forgetCopy(pid, c.path), true), 'Copy forgotten; its files stay on disk');
    busy = '';
    if (done) copies.data = (copies.data ?? []).filter((x) => x.path !== c.path);
  }
</script>

{#if (copies.data ?? []).length > 0}
  <section class="card panel-pad copies" aria-label="Downloaded copies on this machine">
    <h2 class="h">Downloaded copies on this machine</h2>
    <p class="hint">
      Folders downloaded here from the hub. Detach stops one from syncing; Forget also drops blirp's tracking of it. Neither deletes
      any file.
    </p>
    <ul class="list">
      {#each copies.data ?? [] as c (c.path)}
        <li class="row wrap copy">
          <div class="main">
            <span class="mono ellipsis" title={c.path}>{c.path}</span>
            <span class="small muted">
              <span class="badge">{MODE[c.mode]}</span>
              {#if c.workspace}<span class="badge">blirp workspace</span>{/if}
              {#if !c.registered}<span class="badge">not a folder of the project</span>{/if}
              {#if c.origin_machine}copy of {c.origin_machine}: <span class="mono">{c.origin_path}</span> ·{/if}
              downloaded {formatRelative(c.created_at)}
            </span>
          </div>
          {#if app.control}
            <div class="acts">
              {#if c.mode !== 'detached'}
                <button type="button" class="btn sm" disabled={busy === c.path} onclick={() => detach(c)}
                  ><Unlink size={14} aria-hidden="true" />Detach…</button
                >
              {/if}
              {#if !c.workspace}
                <button type="button" class="btn sm ghost" disabled={busy === c.path} onclick={() => forget(c)}
                  ><EyeOff size={14} aria-hidden="true" />Forget…</button
                >
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  </section>
{:else if copies.error}
  <p class="err small" role="alert">Could not list this machine's downloaded copies: {copies.error}</p>
{/if}

<style>
  .copies {
    margin-bottom: 16px;
  }
  .h {
    font-size: 15px;
    margin: 0;
  }
  .hint {
    margin: 4px 0 8px;
  }
  .copy {
    gap: 10px;
    padding: 8px 0;
  }
  .main {
    display: grid;
    gap: 4px;
    flex: 1;
    min-width: 0;
  }
  .acts {
    display: inline-flex;
    gap: 6px;
  }
  .err {
    color: var(--danger);
  }
</style>
