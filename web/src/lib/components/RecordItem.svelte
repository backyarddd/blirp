<script lang="ts" module>
  import type { RecordKind } from '../api/types.gen';

  export const KIND_LABEL: Record<RecordKind, string> = {
    decision: 'Decision',
    plan: 'Plan',
    note: 'Note',
    open_thread: 'Open thread',
    gotcha: 'Gotcha',
  };
</script>

<script lang="ts">
  import Pin from '@lucide/svelte/icons/pin';
  import PinOff from '@lucide/svelte/icons/pin-off';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Check from '@lucide/svelte/icons/check';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import { api } from '../api/client';
  import type { PatchRecord, Record as MemoryRecord } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { formatRelative } from '../time';
  import { href } from '../router';
  import Markdown from './Markdown.svelte';

  interface Props {
    record: MemoryRecord;
    onchange: (r: MemoryRecord | null) => void;
    showKind?: boolean;
  }

  let { record, onchange, showKind = false }: Props = $props();

  let editing = $state(false);
  let title = $state('');
  let body = $state('');
  let busy = $state(false);

  async function patch(p: PatchRecord): Promise<void> {
    busy = true;
    const r = await app.act(() => api.projects.updateRecord(record.project_id, record.id, p));
    busy = false;
    if (r) {
      editing = false;
      onchange(r);
    }
  }

  async function remove(): Promise<void> {
    if (!confirm(`Delete "${record.title}"? This cannot be undone.`)) return;
    busy = true;
    const ok = await app.act(async () => {
      await api.projects.deleteRecord(record.project_id, record.id);
      return true;
    });
    busy = false;
    if (ok) onchange(null);
  }

  function edit(): void {
    title = record.title;
    body = record.body;
    editing = true;
  }
</script>

<article class="rec" class:resolved={record.status !== 'active'}>
  {#if editing}
    <form
      onsubmit={(e) => {
        e.preventDefault();
        void patch({ title: title.trim(), body });
      }}
    >
      <label class="field"><span>Title</span><input class="input" bind:value={title} required /></label>
      <label class="field"><span>Details</span><textarea class="textarea" rows="4" bind:value={body}></textarea></label>
      <div class="row">
        <span class="spacer"></span>
        <button type="button" class="btn sm" onclick={() => (editing = false)} disabled={busy}>Cancel</button>
        <button type="submit" class="btn sm primary" disabled={busy || !title.trim()}>Save</button>
      </div>
    </form>
  {:else}
    <div class="row top">
      {#if record.pinned}<span class="pinned" title="Pinned"><Pin size={13} aria-label="Pinned" /></span>{/if}
      <strong class="title">{record.title}</strong>
      {#if showKind}<span class="badge">{KIND_LABEL[record.kind]}</span>{/if}
      {#if record.status !== 'active'}<span class="badge">{record.status}</span>{/if}
      <span class="spacer"></span>
      <div class="acts">
        <button
          type="button"
          class="icon-btn sm"
          aria-label={record.pinned ? 'Unpin' : 'Pin'}
          title={record.pinned ? 'Unpin' : 'Pin'}
          disabled={busy}
          onclick={() => patch({ pinned: !record.pinned })}
        >
          {#if record.pinned}<PinOff size={14} />{:else}<Pin size={14} />{/if}
        </button>
        {#if record.status === 'active'}
          <button type="button" class="icon-btn sm" aria-label="Mark resolved" title="Mark resolved" disabled={busy} onclick={() => patch({ status: 'resolved' })}>
            <Check size={14} />
          </button>
        {:else}
          <button type="button" class="icon-btn sm" aria-label="Reopen" title="Reopen" disabled={busy} onclick={() => patch({ status: 'active' })}>
            <RotateCcw size={14} />
          </button>
        {/if}
        <button type="button" class="icon-btn sm" aria-label="Edit" title="Edit" disabled={busy} onclick={edit}><Pencil size={14} /></button>
        <button type="button" class="icon-btn sm" aria-label="Delete" title="Delete" disabled={busy} onclick={remove}><Trash size={14} /></button>
      </div>
    </div>
    {#if record.body.trim()}<Markdown source={record.body} class="body small" />{/if}
    <div class="meta hint">
      {record.updated_by} · {formatRelative(record.updated_at)}
      {#if record.source_session_id}· <a href={href.sessions(record.source_session_id)}>source session</a>{/if}
    </div>
  {/if}
</article>

<style>
  .rec {
    padding: 10px 0;
  }
  .rec.resolved .title {
    color: var(--text-2);
    text-decoration: line-through;
  }
  .top {
    gap: 6px;
    align-items: flex-start;
  }
  .title {
    font-weight: 600;
    min-width: 0;
    overflow-wrap: anywhere;
    padding-top: 2px;
  }
  .acts {
    display: flex;
    gap: 2px;
    flex: none;
  }
  .pinned {
    color: var(--accent);
    display: inline-flex;
    padding-top: 4px;
  }
  .rec :global(.body) {
    margin-top: 4px;
    color: var(--text-2);
  }
  .meta {
    margin-top: 4px;
  }
</style>
