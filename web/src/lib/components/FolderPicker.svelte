<script lang="ts">
  import { untrack } from 'svelte';
  import CornerLeftUp from '@lucide/svelte/icons/corner-left-up';
  import Folder from '@lucide/svelte/icons/folder';
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import { api, errorMessage } from '../api/client';
  import type { MachineDirs } from '../api/types.gen';

  /**
   * Browse folders on a machine (this one or another paired one, relayed by the hub). Only folder
   * names inside that machine's home are listed; files never are.
   */
  interface Props {
    machineId: string;
    machineName: string;
    /** Folder to open first (absolute, on that machine); home when empty or outside it. */
    start?: string;
    pickLabel?: string;
    onpick: (path: string) => void;
  }

  let { machineId, machineName, start = '', pickLabel = 'Use this folder', onpick }: Props = $props();

  let listing: MachineDirs | null = $state.raw(null);
  let hidden = $state(false);
  let loading = $state(false);
  let error: string | null = $state(null);
  let seq = 0;

  async function load(path: string): Promise<void> {
    const mine = ++seq;
    loading = true;
    error = null;
    try {
      const l = await api.machines.dirs(machineId, path, hidden);
      if (mine === seq) listing = l;
    } catch (e) {
      if (mine !== seq) return;
      // A start folder that no longer exists (or lies outside home) falls back to home once.
      if (path !== '' && listing === null) {
        void load('');
        return;
      }
      error = errorMessage(e);
    } finally {
      if (mine === seq) loading = false;
    }
  }

  $effect(() => {
    void machineId;
    untrack(() => {
      listing = null;
      void load(start);
    });
  });

  function toggleHidden(): void {
    hidden = !hidden;
    void load(listing?.path ?? '');
  }
</script>

<div class="picker" aria-busy={loading}>
  <div class="bar">
    <button
      type="button"
      class="icon-btn sm"
      aria-label="Parent folder"
      title="Parent folder"
      disabled={!listing?.parent || loading}
      onclick={() => listing?.parent && load(listing.parent)}><CornerLeftUp size={14} /></button
    >
    <span class="where mono ellipsis" title={listing?.path ?? ''}>{listing?.path ?? `Home on ${machineName}`}</span>
    <label class="check small">
      <input type="checkbox" checked={hidden} onchange={toggleHidden} />
      <span>Hidden</span>
    </label>
  </div>
  <ul class="list" aria-label="Folders on {machineName}">
    {#if error}
      <li class="msg error" role="alert">{error}</li>
    {:else if listing && listing.entries.length === 0}
      <li class="msg faint">No folders here.</li>
    {:else if !listing}
      <li class="msg faint" role="status">Loading folders on {machineName}…</li>
    {/if}
    {#each listing?.entries ?? [] as d (d.path)}
      <li>
        <button type="button" class="dir" onclick={() => load(d.path)} disabled={loading}>
          {#if d.is_git}<GitBranch size={13} aria-label="git repository" />{:else}<Folder size={13} aria-hidden="true" />{/if}
          <span class="ellipsis">{d.name}</span>
        </button>
      </li>
    {/each}
    {#if listing?.truncated}<li class="msg faint">Only the first folders are shown.</li>{/if}
  </ul>
  <div class="foot">
    <button type="button" class="btn sm primary" disabled={!listing || loading} onclick={() => listing && onpick(listing.path)}>
      {pickLabel}
    </button>
  </div>
</div>

<style>
  .picker {
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    overflow: hidden;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-bottom: 1px solid var(--border);
    background: var(--panel-2);
  }
  .where {
    flex: 1;
    min-width: 0;
    font-size: 12px;
  }
  .check.small {
    margin: 0;
    font-size: 12px;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 4px;
    max-height: 220px;
    overflow: auto;
  }
  .dir {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    padding: 5px 8px;
    border: 0;
    border-radius: 6px;
    background: none;
    color: var(--text);
    text-align: left;
    cursor: pointer;
    font: inherit;
    font-size: 13px;
  }
  .dir:hover:not(:disabled) {
    background: var(--panel-2);
  }
  .msg {
    padding: 8px;
    font-size: 12.5px;
  }
  .msg.error {
    color: var(--danger);
  }
  .foot {
    display: flex;
    justify-content: flex-end;
    padding: 6px 8px;
    border-top: 1px solid var(--border);
  }
</style>
