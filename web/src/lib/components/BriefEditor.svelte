<script lang="ts">
  import Pencil from '@lucide/svelte/icons/pencil';
  import { api } from '../api/client';
  import type { Brief } from '../api/types';
  import { app } from '../app.svelte';
  import { formatRelative } from '../time';
  import Markdown from './Markdown.svelte';

  interface Props {
    projectId: string;
    brief: Brief | null;
    onsaved: (b: Brief) => void;
    compact?: boolean;
  }

  let { projectId, brief, onsaved, compact = false }: Props = $props();

  let editing = $state(false);
  let preview = $state(false);
  let draft = $state('');
  let saving = $state(false);

  function edit(): void {
    draft = brief?.body_md ?? '';
    preview = false;
    editing = true;
  }

  async function save(): Promise<void> {
    saving = true;
    const b = await app.act(() => api.projects.putBrief(projectId, draft), 'Brief saved');
    saving = false;
    if (b) {
      editing = false;
      onsaved(b);
    }
  }
</script>

<div class="brief" class:compact>
  {#if editing}
    <div class="pills" role="tablist" aria-label="Brief editor mode">
      <button type="button" class="pill" role="tab" aria-selected={!preview} onclick={() => (preview = false)}>Write</button>
      <button type="button" class="pill" role="tab" aria-selected={preview} onclick={() => (preview = true)}>Preview</button>
    </div>
    {#if preview}
      <div class="preview"><Markdown source={draft || '_Empty brief_'} /></div>
    {:else}
      <textarea class="textarea mono" rows={compact ? 10 : 16} bind:value={draft} aria-label="Brief markdown"></textarea>
    {/if}
    <div class="row wrap actions">
      <span class="hint">Markdown. Injected at the top of every new session in this project.</span>
      <span class="spacer"></span>
      <button type="button" class="btn sm" onclick={() => (editing = false)} disabled={saving}>Cancel</button>
      <button type="button" class="btn sm primary" onclick={save} disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
    </div>
  {:else}
    <div class="row head">
      {#if brief}
        <span class="hint">v{brief.version} · {brief.updated_by} · {formatRelative(brief.updated_at)}</span>
      {/if}
      <span class="spacer"></span>
      <button type="button" class="btn sm ghost" onclick={edit}><Pencil size={14} aria-hidden="true" />Edit</button>
    </div>
    {#if brief?.body_md.trim()}
      <Markdown source={brief.body_md} />
    {:else}
      <p class="muted empty">No brief yet. blirp writes one after the first session here is distilled, or you can write it now.</p>
    {/if}
  {/if}
</div>

<style>
  .head {
    margin-bottom: 6px;
  }
  .actions {
    margin-top: 8px;
  }
  .preview {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 10px 12px;
    min-height: 120px;
    margin-top: 8px;
  }
  textarea {
    margin-top: 8px;
  }
  .empty {
    margin: 0;
  }
  .compact :global(.md) {
    font-size: 13px;
  }
</style>
