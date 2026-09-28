<script lang="ts">
  import Check from '@lucide/svelte/icons/check';
  import Archive from '@lucide/svelte/icons/archive';
  import ArchiveRestore from '@lucide/svelte/icons/archive-restore';
  import FolderInput from '@lucide/svelte/icons/folder-input';
  import Trash from '@lucide/svelte/icons/trash-2';
  import type { Record as MemoryRecord } from '../api/types.gen';
  import { dialogs } from '../dialogs.svelte';
  import { deleteRecords, setRecordsStatus } from '../manage';
  import type { Selection } from '../selection.svelte';

  /** `shown`: the records the list shows, in its order. Only rendered with control. */
  let { selection, shown }: { selection: Selection; shown: MemoryRecord[] } = $props();

  const order = $derived(shown.map((r) => r.id));
  const picked = $derived(shown.filter((r) => selection.ids.has(r.id)));
  const all = $derived(order.length > 0 && picked.length === order.length);
  const allArchived = $derived(picked.length > 0 && picked.every((r) => r.status === 'archived'));

  // A record that left the list (deleted, moved, filtered out) leaves the selection.
  $effect(() => selection.retain(order));

  async function remove(): Promise<void> {
    if (await deleteRecords(picked)) selection.ids.clear();
  }
</script>

<div class="bulk" role="toolbar" aria-label="Selected records">
  <label class="all">
    <input type="checkbox" checked={all} indeterminate={picked.length > 0 && !all} onchange={() => selection.toggleAll(order)} />
    <span>{picked.length} selected</span>
  </label>
  <div class="acts">
    <button
      type="button"
      class="icon-btn sm"
      aria-label="Resolve selected"
      title="Mark resolved"
      disabled={picked.length === 0}
      onclick={() => setRecordsStatus(picked, 'resolved')}><Check size={15} /></button
    >
    <button
      type="button"
      class="icon-btn sm"
      aria-label={allArchived ? 'Unarchive selected' : 'Archive selected'}
      title={allArchived ? 'Unarchive' : 'Archive'}
      disabled={picked.length === 0}
      onclick={() => setRecordsStatus(picked, allArchived ? 'active' : 'archived')}
      >{#if allArchived}<ArchiveRestore size={15} />{:else}<Archive size={15} />{/if}</button
    >
    <button
      type="button"
      class="icon-btn sm"
      aria-label="Move selected"
      title="Move to project…"
      disabled={picked.length === 0}
      onclick={() => (dialogs.movingRecords = picked)}><FolderInput size={15} /></button
    >
    <button type="button" class="icon-btn sm danger" aria-label="Delete selected" title="Delete…" disabled={picked.length === 0} onclick={remove}
      ><Trash size={15} /></button
    >
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
