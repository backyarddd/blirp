<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Trash from '@lucide/svelte/icons/trash-2';
  import ExternalLink from '@lucide/svelte/icons/external-link';
  import { api } from '../../lib/api/client';
  import type { CreateResource, ProjectSummary, Resource as Res, ResourceKind } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const list = new Resource(() => api.projects.resources(pid));
  $effect(() => {
    void list.load();
  });

  const KINDS: { id: ResourceKind; label: string }[] = [
    { id: 'link', label: 'Link' },
    { id: 'repo', label: 'Repository' },
    { id: 'pr', label: 'Pull request' },
    { id: 'issue', label: 'Issue' },
    { id: 'doc', label: 'Doc' },
    { id: 'file', label: 'File' },
  ];
  const kindLabel = (k: ResourceKind): string => KINDS.find((x) => x.id === k)?.label ?? k;

  // Only web URLs become clickable links; anything else (file paths, custom schemes) is shown as text.
  const isWeb = (url: string): boolean => /^https?:\/\//i.test(url);

  let editingId: string | 'new' | null = $state(null);
  let form: CreateResource = $state({ kind: 'link', url: '', title: '' });
  let saving = $state(false);

  function startNew(): void {
    form = { kind: 'link', url: '', title: '' };
    editingId = 'new';
  }

  function startEdit(r: Res): void {
    form = { kind: r.kind, url: r.url, title: r.title };
    editingId = r.id;
  }

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const input: CreateResource = { kind: form.kind, url: form.url.trim(), title: form.title.trim() || form.url.trim() };
    if (!input.url) return;
    saving = true;
    const id = editingId;
    const saved = await app.act(() =>
      id && id !== 'new' ? api.projects.updateResource(pid, id, input) : api.projects.createResource(pid, input),
    );
    saving = false;
    if (saved) {
      const rest = (list.data ?? []).filter((r) => r.id !== saved.id);
      list.data = [saved, ...rest];
      editingId = null;
    }
  }

  async function remove(r: Res): Promise<void> {
    if (!confirm(`Remove "${r.title}"?`)) return;
    const ok = await app.act(async () => {
      await api.projects.deleteResource(pid, r.id);
      return true;
    });
    if (ok) list.data = (list.data ?? []).filter((x) => x.id !== r.id);
  }
</script>

{#snippet editor()}
  <form class="editor" onsubmit={save}>
    <div class="row wrap">
      <select class="select kind" bind:value={form.kind} aria-label="Kind">
        {#each KINDS as k (k.id)}<option value={k.id}>{k.label}</option>{/each}
      </select>
      <input class="input grow" placeholder="https://… or a path" aria-label="URL or path" bind:value={form.url} required />
    </div>
    <input class="input" placeholder="Title (optional)" aria-label="Title" bind:value={form.title} />
    <div class="row">
      <span class="spacer"></span>
      <button type="button" class="btn sm" onclick={() => (editingId = null)} disabled={saving}>Cancel</button>
      <button type="submit" class="btn sm primary" disabled={saving || !form.url.trim()}>Save</button>
    </div>
  </form>
{/snippet}

<section class="card panel-pad">
  <div class="row">
    <h2 class="h">Resources</h2>
    <span class="spacer"></span>
    {#if app.control}<button type="button" class="btn sm primary" onclick={startNew}><Plus size={14} aria-hidden="true" />Add resource</button>{/if}
  </div>
  <p class="hint">Links and references that belong to this project: repos, PRs, issues, docs.</p>
  {#if editingId === 'new'}{@render editor()}{/if}
  <Loadable
    loading={list.loading}
    error={list.error}
    empty={(list.data?.length ?? 0) === 0}
    emptyText="No resources yet."
    onretry={() => list.load()}
  >
    <ul class="list">
      {#each list.data ?? [] as r (r.id)}
        <li class="res">
          {#if editingId === r.id}
            {@render editor()}
          {:else}
            <span class="badge">{kindLabel(r.kind)}</span>
            <span class="main">
              {#if isWeb(r.url)}
                <a href={r.url} target="_blank" rel="noopener noreferrer" class="title">{r.title}<ExternalLink size={12} aria-hidden="true" /></a>
              {:else}
                <span class="title">{r.title}</span>
              {/if}
              <span class="url mono small faint ellipsis" title={r.url}>{r.url}</span>
            </span>
            {#if app.control}
              <button type="button" class="icon-btn sm" aria-label="Edit {r.title}" onclick={() => startEdit(r)}><Pencil size={14} /></button>
              <button type="button" class="icon-btn sm" aria-label="Remove {r.title}" onclick={() => remove(r)}><Trash size={14} /></button>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
  </Loadable>
</section>

<style>
  .h {
    font-size: 15px;
    margin: 0;
  }
  .res {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 0;
  }
  .main {
    display: grid;
    flex: 1;
    min-width: 0;
  }
  .title {
    font-weight: 600;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    text-decoration: none;
  }
  a.title:hover {
    color: var(--accent);
  }
  .editor {
    display: grid;
    gap: 8px;
    flex: 1;
    padding: 12px;
    margin: 8px 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--panel-2);
  }
  .kind {
    width: auto;
  }
  .grow {
    flex: 1;
    min-width: 200px;
    width: auto;
  }
</style>
