<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Trash from '@lucide/svelte/icons/trash-2';
  import { api, errorMessage } from '../../lib/api/client';
  import type { Project, WikiPage } from '../../lib/api/types';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { navigate } from '../../lib/router.svelte';
  import { href } from '../../lib/router';
  import { slugify } from '../../lib/markdown';
  import { formatRelative } from '../../lib/time';
  import Loadable from '../../lib/components/Loadable.svelte';
  import Markdown from '../../lib/components/Markdown.svelte';

  let { project, slug }: { project: Project; slug: string | null } = $props();
  const pid = $derived(project.id);

  const pages = new Resource(() => api.projects.wiki(pid));
  $effect(() => {
    void app.memoryTick[pid];
    void pages.reload();
  });

  const sorted = $derived([...(pages.data ?? [])].sort((a, b) => a.title.localeCompare(b.title)));
  const current = $derived(slug ? (pages.data ?? []).find((p) => p.slug === slug) : undefined);

  let mode: 'view' | 'new' | 'edit' = $state('view');
  let title = $state('');
  let pageSlug = $state('');
  let slugTouched = $state(false);
  let body = $state('');
  let preview = $state(false);
  let saving = $state(false);
  let formError: string | null = $state(null);

  $effect(() => {
    void slug;
    mode = 'view';
  });

  function startNew(): void {
    title = '';
    pageSlug = '';
    slugTouched = false;
    body = '';
    preview = false;
    formError = null;
    mode = 'new';
  }

  function startEdit(p: WikiPage): void {
    title = p.title;
    pageSlug = p.slug;
    slugTouched = true;
    body = p.body_md;
    preview = false;
    formError = null;
    mode = 'edit';
  }

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const input = { title: title.trim(), slug: (slugTouched ? pageSlug : slugify(title)).trim(), body_md: body };
    if (!input.title || !input.slug) {
      formError = 'A title (and a slug made of letters or numbers) is required.';
      return;
    }
    saving = true;
    formError = null;
    try {
      const saved =
        mode === 'edit' && current
          ? await api.projects.updateWiki(pid, current.slug, input)
          : await api.projects.createWiki(pid, input);
      const rest = (pages.data ?? []).filter((p) => p.id !== saved.id);
      pages.data = [...rest, saved];
      mode = 'view';
      navigate(href.project(pid, 'wiki', saved.slug));
    } catch (err) {
      formError = errorMessage(err);
    } finally {
      saving = false;
    }
  }

  async function remove(p: WikiPage): Promise<void> {
    if (!confirm(`Delete the wiki page "${p.title}"?`)) return;
    const ok = await app.act(async () => {
      await api.projects.deleteWiki(pid, p.slug);
      return true;
    });
    if (ok) {
      pages.data = (pages.data ?? []).filter((x) => x.id !== p.id);
      navigate(href.project(pid, 'wiki'));
    }
  }
</script>

<div class="wiki">
  <aside class="card side">
    <div class="row side-head">
      <h2 class="h">Pages</h2>
      <span class="spacer"></span>
      <button type="button" class="icon-btn sm" aria-label="New page" title="New page" onclick={startNew}><Plus size={15} /></button>
    </div>
    <Loadable
      loading={pages.loading}
      error={pages.error}
      empty={sorted.length === 0}
      emptyText="No pages yet."
      onretry={() => pages.load()}
    >
      <ul class="pages">
        {#each sorted as p (p.id)}
          <li>
            <a href={href.project(pid, 'wiki', p.slug)} aria-current={p.slug === slug ? 'page' : undefined} class="ellipsis">{p.title}</a>
          </li>
        {/each}
      </ul>
    </Loadable>
  </aside>

  <section class="card content">
    {#if mode !== 'view'}
      <form onsubmit={save}>
        <div class="row wrap">
          <label class="field grow">
            <span>Title</span>
            <input class="input" bind:value={title} required />
          </label>
          <label class="field slug">
            <span>Slug</span>
            <input
              class="input mono"
              value={slugTouched ? pageSlug : slugify(title)}
              oninput={(e) => {
                slugTouched = true;
                pageSlug = e.currentTarget.value;
              }}
              pattern="[a-z0-9-]+"
              title="Lowercase letters, digits and dashes"
            />
          </label>
        </div>
        <div class="pills" role="tablist" aria-label="Editor mode">
          <button type="button" class="pill" role="tab" aria-selected={!preview} onclick={() => (preview = false)}>Write</button>
          <button type="button" class="pill" role="tab" aria-selected={preview} onclick={() => (preview = true)}>Preview</button>
        </div>
        {#if preview}
          <div class="preview"><Markdown source={body || '_Empty page_'} /></div>
        {:else}
          <textarea class="textarea mono editor" rows="20" bind:value={body} aria-label="Page markdown"></textarea>
        {/if}
        {#if formError}<p class="err" role="alert">{formError}</p>{/if}
        <div class="row actions">
          <span class="spacer"></span>
          <button type="button" class="btn" onclick={() => (mode = 'view')} disabled={saving}>Cancel</button>
          <button type="submit" class="btn primary" disabled={saving}>{saving ? 'Saving…' : 'Save page'}</button>
        </div>
      </form>
    {:else if current}
      <div class="row wrap page-head">
        <h2 class="title">{current.title}</h2>
        <span class="spacer"></span>
        <span class="hint">{current.updated_by} · {formatRelative(current.updated_at)}</span>
        <button type="button" class="btn sm" onclick={() => startEdit(current)}><Pencil size={14} aria-hidden="true" />Edit</button>
        <button type="button" class="btn sm ghost" onclick={() => remove(current)}><Trash size={14} aria-hidden="true" />Delete</button>
      </div>
      <Markdown source={current.body_md || '_This page is empty._'} />
    {:else if slug && pages.data}
      <p class="muted">No page named <code>{slug}</code>.</p>
      <button type="button" class="btn" onclick={startNew}>Create a page</button>
    {:else}
      <div class="intro">
        <h2 class="h">Project wiki</h2>
        <p class="muted">Longer-lived docs for this project: setup, architecture notes, runbooks. Agents can read them through blirp's memory tools.</p>
        <button type="button" class="btn primary" onclick={startNew}><Plus size={14} aria-hidden="true" />New page</button>
      </div>
    {/if}
  </section>
</div>

<style>
  .wiki {
    display: grid;
    grid-template-columns: 240px minmax(0, 1fr);
    gap: 16px;
    align-items: start;
  }
  .side {
    padding: 12px;
  }
  .side-head {
    margin-bottom: 6px;
  }
  .h {
    font-size: 15px;
    margin: 0;
  }
  .pages {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .pages a {
    display: block;
    padding: 6px 8px;
    border-radius: 6px;
    text-decoration: none;
    color: var(--text-2);
  }
  .pages a:hover {
    background: var(--panel-2);
    color: var(--text);
  }
  .pages a[aria-current='page'] {
    background: var(--accent-soft);
    color: var(--accent);
    font-weight: 600;
  }
  .content {
    padding: 20px;
    min-height: 300px;
  }
  .grow {
    flex: 1;
    min-width: 200px;
  }
  .slug {
    width: 220px;
  }
  .editor,
  .preview {
    margin-top: 10px;
  }
  .preview {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 12px 14px;
    min-height: 300px;
  }
  .actions {
    margin-top: 12px;
  }
  .page-head {
    margin-bottom: 12px;
  }
  .title {
    margin: 0;
    font-size: 20px;
  }
  .intro {
    display: grid;
    gap: 8px;
    justify-items: start;
  }
  .intro p {
    margin: 0 0 6px;
  }
  .err {
    color: var(--danger);
  }
  @media (max-width: 760px) {
    .wiki {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
