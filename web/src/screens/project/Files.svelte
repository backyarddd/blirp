<script lang="ts">
  import { untrack } from 'svelte';
  import { SvelteMap } from 'svelte/reactivity';
  import Folder from '@lucide/svelte/icons/folder';
  import FolderOpen from '@lucide/svelte/icons/folder-open';
  import FileIcon from '@lucide/svelte/icons/file';
  import { api, errorMessage } from '../../lib/api/client';
  import type { FileEntry, ProjectSummary } from '../../lib/api/types.gen';
  import { Resource } from '../../lib/resource.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';
  import LocalCopies from './LocalCopies.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  interface DirState {
    entries: FileEntry[] | null;
    error: string | null;
  }

  // Multi-folder projects browse one folder of this machine at a time (`root=`).
  const roots = $derived(project.paths.filter((p) => p.local).map((p) => p.path));
  let root: string | undefined = $state(untrack(() => roots[0]));

  const dirs = new SvelteMap<string, DirState>();
  const expanded = new SvelteMap<string, boolean>();
  let selected: string | null = $state(null);

  async function loadDir(path: string): Promise<void> {
    dirs.set(path, { entries: null, error: null });
    try {
      // The daemon sorts directories first, then by name.
      const listing = await api.projects.files(pid, path, { root });
      dirs.set(path, { entries: listing.entries, error: null });
    } catch (e) {
      dirs.set(path, { entries: null, error: errorMessage(e) });
    }
  }

  // untracked: loadDir writes the SvelteMaps, which must not become dependencies of this effect.
  $effect(() => {
    void pid;
    void root;
    untrack(() => {
      dirs.clear();
      expanded.clear();
      selected = null;
      void loadDir('');
    });
  });

  function toggle(entry: FileEntry): void {
    const open = !expanded.get(entry.path);
    expanded.set(entry.path, open);
    if (open && !dirs.has(entry.path)) void loadDir(entry.path);
  }

  const content = new Resource(() =>
    selected ? api.projects.fileContent(pid, selected, { root }) : Promise.resolve(null),
  );
  $effect(() => {
    void content.load();
  });

  const lines = $derived(content.data ? content.data.content.split('\n') : []);
</script>

{#snippet tree(path: string, depth: number)}
  {@const state = dirs.get(path)}
  {#if !state || (state.entries === null && !state.error)}
    <li class="note" style:padding-left="{depth * 14 + 8}px">Loading…</li>
  {:else if state.error}
    <li class="note err" style:padding-left="{depth * 14 + 8}px">
      {state.error} <button type="button" class="btn sm" onclick={() => loadDir(path)}>Retry</button>
    </li>
  {:else if state.entries && state.entries.length === 0}
    <li class="note" style:padding-left="{depth * 14 + 8}px">Empty folder</li>
  {:else}
    {#each state.entries ?? [] as entry (entry.path)}
      <li>
        {#if entry.kind === 'dir'}
          <button
            type="button"
            class="node"
            style:padding-left="{depth * 14 + 8}px"
            aria-expanded={expanded.get(entry.path) === true}
            onclick={() => toggle(entry)}
          >
            {#if expanded.get(entry.path)}<FolderOpen size={14} aria-hidden="true" />{:else}<Folder size={14} aria-hidden="true" />{/if}
            <span class="ellipsis">{entry.name}</span>
          </button>
          {#if expanded.get(entry.path)}
            <ul class="tree" role="group">{@render tree(entry.path, depth + 1)}</ul>
          {/if}
        {:else}
          <button
            type="button"
            class="node"
            class:selected={selected === entry.path}
            aria-current={selected === entry.path ? 'true' : undefined}
            style:padding-left="{depth * 14 + 8}px"
            onclick={() => (selected = entry.path)}
          >
            <FileIcon size={14} aria-hidden="true" />
            <span class="ellipsis">{entry.name}</span>
          </button>
        {/if}
      </li>
    {/each}
  {/if}
{/snippet}

<LocalCopies {project} />

{#if roots.length > 1}
  <label class="field roots">
    <span>Folder</span>
    <select class="select mono" bind:value={root}>
      {#each roots as r (r)}<option value={r}>{r}</option>{/each}
    </select>
  </label>
{/if}

<div class="files">
  <nav class="card side" aria-label="Project files">
    <ul class="tree">{@render tree('', 0)}</ul>
  </nav>
  <section class="card viewer" aria-label="File viewer">
    {#if !selected}
      <p class="muted pad">Select a file to view it. Files are read-only here.</p>
    {:else}
      <header class="vhead mono small ellipsis">{selected}</header>
      <Loadable loading={content.loading} error={content.error} empty={!content.data} onretry={() => content.load()}>
        <!-- Binary and >1 MiB files arrive as errors (415 binary_file, 413 file_too_large). -->
        <div class="code">
          <pre class="nums" aria-hidden="true">{lines.map((_, i) => i + 1).join('\n')}</pre>
          <pre class="text">{content.data?.content}</pre>
        </div>
      </Loadable>
    {/if}
  </section>
</div>

<style>
  .files {
    display: grid;
    grid-template-columns: 280px minmax(0, 1fr);
    gap: 16px;
    align-items: start;
  }
  .side {
    padding: 6px;
    max-height: calc(100vh - 260px);
    overflow: auto;
  }
  .tree {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .node {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    padding: 4px 8px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    text-align: left;
    cursor: pointer;
    color: var(--text-2);
    font-size: 13px;
  }
  .node:hover {
    background: var(--panel-2);
    color: var(--text);
  }
  .node.selected {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .note {
    font-size: 12.5px;
    color: var(--text-3);
    padding: 4px 8px;
  }
  .note.err {
    color: var(--danger);
  }
  .viewer {
    min-height: 300px;
    overflow: hidden;
  }
  .vhead {
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
    color: var(--text-2);
  }
  .pad {
    padding: 16px;
  }
  .roots {
    max-width: 520px;
  }
  .code {
    display: flex;
    overflow: auto;
    max-height: calc(100vh - 300px);
  }
  .code pre {
    margin: 0;
    padding: 10px 12px;
    font-size: 12.5px;
    line-height: 1.5;
  }
  .nums {
    text-align: right;
    color: var(--text-3);
    user-select: none;
    border-right: 1px solid var(--border);
    background: var(--panel-2);
    flex: none;
  }
  .text {
    flex: 1;
    white-space: pre;
  }
  @media (max-width: 760px) {
    .files {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
