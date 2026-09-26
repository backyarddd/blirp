<script lang="ts">
  import { untrack } from 'svelte';
  import GitBranch from '@lucide/svelte/icons/git-branch';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { api } from '../../lib/api/client';
  import type { GitStatusEntry, ProjectSummary } from '../../lib/api/types.gen';
  import { Resource } from '../../lib/resource.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  // A project may hold several repos on this machine; `root=` picks one.
  const roots = $derived(project.paths.filter((p) => p.local && p.is_git).map((p) => p.path));
  let root: string | undefined = $state(untrack(() => roots[0]));

  let selected: string | null = $state(null);
  const status = new Resource(() => api.projects.git(pid, { root }));
  $effect(() => {
    void root;
    untrack(() => (selected = null));
    void status.load();
  });

  const diff = new Resource(() => (selected ? api.projects.gitDiff(pid, selected, { root }) : Promise.resolve(null)));
  $effect(() => {
    void diff.load();
  });

  function lineClass(line: string): string {
    if (line.startsWith('+++') || line.startsWith('---')) return 'meta';
    if (line.startsWith('@@')) return 'hunk';
    if (line.startsWith('+')) return 'add';
    if (line.startsWith('-')) return 'del';
    return '';
  }

  /** Porcelain v2 letters: `.` unchanged, `?` untracked; show the familiar short form. */
  function statusCode(e: GitStatusEntry): string {
    if (e.index === '?') return '??';
    return `${e.index}${e.worktree}`.replaceAll('.', '');
  }

  function statusLabel(e: GitStatusEntry): string {
    const c = statusCode(e);
    if (e.conflicted) return 'conflict';
    if (c === '??') return 'untracked';
    if (c.includes('A')) return 'added';
    if (c.includes('D')) return 'deleted';
    if (c.includes('R')) return 'renamed';
    if (c.includes('M')) return 'modified';
    return 'changed';
  }
</script>

{#if roots.length > 1}
  <label class="field roots">
    <span>Repository</span>
    <select class="select mono" bind:value={root}>
      {#each roots as r (r)}<option value={r}>{r}</option>{/each}
    </select>
  </label>
{/if}

<Loadable loading={status.loading} error={status.error} empty={!status.data} onretry={() => status.load()}>
  {#if status.data && !status.data.is_git}
    <p class="muted">This folder is not a git repository.</p>
  {:else if status.data}
    {@const s = status.data}
    <div class="row wrap head">
      <span class="badge"><GitBranch size={12} aria-hidden="true" />{s.branch ?? 'detached HEAD'}</span>
      {#if s.ahead}<span class="small muted">{s.ahead} ahead</span>{/if}
      {#if s.behind}<span class="small muted">{s.behind} behind</span>{/if}
      <span class="small muted">{s.entries.length} changed {s.entries.length === 1 ? 'file' : 'files'}</span>
      <span class="spacer"></span>
      <button type="button" class="btn sm" onclick={() => status.reload()} disabled={status.loading}><RefreshCw size={14} aria-hidden="true" />Refresh</button>
    </div>
    <div class="git">
      <nav class="card side" aria-label="Changed files">
        {#if s.entries.length === 0}
          <p class="muted pad">Working tree clean.</p>
        {:else}
          <ul class="changes">
            {#each s.entries as f (f.path)}
              <li>
                <button
                  type="button"
                  class="file"
                  class:selected={selected === f.path}
                  aria-current={selected === f.path ? 'true' : undefined}
                  onclick={() => (selected = f.path)}
                >
                  <span class="code mono {statusLabel(f)}" title={statusLabel(f)}>{statusCode(f) || '·'}</span>
                  <span class="mono ellipsis">{f.path}</span>
                  <span class="sr-only">{statusLabel(f)}</span>
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      </nav>
      <section class="card viewer" aria-label="Diff">
        {#if !selected}
          <p class="muted pad">Select a file to see its diff.</p>
        {:else}
          <header class="vhead mono small ellipsis">{selected}</header>
          <Loadable loading={diff.loading} error={diff.error} empty={!diff.data} onretry={() => diff.load()}>
            {#if diff.data?.diff.trim()}
              <pre class="diff">{#each diff.data.diff.split('\n') as line, i (i)}<span class="ln {lineClass(line)}">{line}{'\n'}</span>{/each}</pre>
            {:else}
              <p class="muted pad">No textual changes (binary file, mode change, or untracked).</p>
            {/if}
          </Loadable>
        {/if}
      </section>
    </div>
  {/if}
</Loadable>

<style>
  .head {
    margin-bottom: 12px;
  }
  .roots {
    max-width: 520px;
  }
  .git {
    display: grid;
    grid-template-columns: 300px minmax(0, 1fr);
    gap: 16px;
    align-items: start;
  }
  .side {
    padding: 6px;
    max-height: calc(100vh - 300px);
    overflow: auto;
  }
  .changes {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .file {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 5px 8px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    cursor: pointer;
    text-align: left;
    font-size: 12.5px;
  }
  .file:hover {
    background: var(--panel-2);
  }
  .file.selected {
    background: var(--accent-soft);
  }
  .code {
    width: 22px;
    flex: none;
    font-weight: 700;
    color: var(--text-2);
  }
  .code.added,
  .code.untracked {
    color: var(--completed);
  }
  .code.deleted,
  .code.conflict {
    color: var(--failed);
  }
  .code.modified,
  .code.renamed {
    color: var(--waiting);
  }
  .viewer {
    overflow: hidden;
    min-height: 300px;
  }
  .vhead {
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
    color: var(--text-2);
  }
  .pad {
    padding: 16px;
    margin: 0;
  }
  .diff {
    margin: 0;
    padding: 8px 0;
    font-size: 12.5px;
    line-height: 1.5;
    overflow: auto;
    max-height: calc(100vh - 320px);
  }
  .ln {
    display: block;
    padding: 0 12px;
    white-space: pre;
  }
  .ln.add {
    background: var(--completed-bg);
    color: var(--completed);
  }
  .ln.del {
    background: var(--failed-bg);
    color: var(--failed);
  }
  .ln.hunk {
    color: var(--text-3);
    background: var(--panel-2);
  }
  .ln.meta {
    color: var(--text-2);
    font-weight: 600;
  }
  @media (max-width: 760px) {
    .git {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
