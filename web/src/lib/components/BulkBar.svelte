<script lang="ts">
  import FolderInput from '@lucide/svelte/icons/folder-input';
  import Pin from '@lucide/svelte/icons/pin';
  import Archive from '@lucide/svelte/icons/archive';
  import Trash from '@lucide/svelte/icons/trash-2';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { dialogs } from '../dialogs.svelte';
  import { deleteSessions, markSessions } from '../manage';
  import { sessionKey } from '../marks';
  import type { Selection } from '../selection.svelte';

  /** `shown`: the sessions the list shows, in its order. */
  let { selection, shown }: { selection: Selection; shown: Session[] } = $props();

  const order = $derived(shown.map((s) => s.id));
  const picked = $derived(shown.filter((s) => selection.ids.has(s.id)));
  const all = $derived(order.length > 0 && picked.length === order.length);
  const allPinned = $derived(picked.length > 0 && picked.every((s) => app.pinned.has(sessionKey(s.id))));
  const allArchived = $derived(picked.length > 0 && picked.every((s) => app.archived.has(sessionKey(s.id))));

  // A session that left the list (deleted, moved out of a project view) leaves the selection.
  $effect(() => selection.retain(order));

  async function remove(): Promise<void> {
    if (await deleteSessions(picked)) selection.ids.clear();
  }
</script>

<div class="bulk" role="toolbar" aria-label="Selected sessions">
  <label class="all">
    <input type="checkbox" checked={all} indeterminate={picked.length > 0 && !all} onchange={() => selection.toggleAll(order)} />
    <span>{picked.length} selected</span>
  </label>
  <div class="acts">
    {#if app.control}
      <button type="button" class="icon-btn sm" aria-label="Move selected" title="Move…" disabled={picked.length === 0} onclick={() => (dialogs.moving = picked)}
        ><FolderInput size={15} /></button
      >
    {/if}
    <button
      type="button"
      class="icon-btn sm"
      aria-label={allPinned ? 'Unpin selected' : 'Pin selected'}
      title={allPinned ? 'Unpin' : 'Pin'}
      disabled={picked.length === 0}
      onclick={() => markSessions('pinned', picked, !allPinned)}><Pin size={15} /></button
    >
    <button
      type="button"
      class="icon-btn sm"
      aria-label={allArchived ? 'Unarchive selected' : 'Archive selected'}
      title={allArchived ? 'Unarchive' : 'Archive'}
      disabled={picked.length === 0}
      onclick={() => markSessions('archived', picked, !allArchived)}><Archive size={15} /></button
    >
    {#if app.control}
      <button type="button" class="icon-btn sm danger" aria-label="Delete selected" title="Delete…" disabled={picked.length === 0} onclick={remove}
        ><Trash size={15} /></button
      >
    {/if}
    <button type="button" class="btn sm" onclick={() => selection.stop()}>Done</button>
  </div>
</div>

<style>
  .bulk {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    flex-wrap: wrap;
  }
  .all {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12.5px;
    font-weight: 600;
  }
  .acts {
    display: inline-flex;
    align-items: center;
    gap: 2px;
  }
  .danger {
    color: var(--danger);
  }
</style>
