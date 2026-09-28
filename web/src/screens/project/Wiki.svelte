<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Trash from '@lucide/svelte/icons/trash-2';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import { api, errorMessage } from '../../lib/api/client';
  import type { ProjectSummary, WikiPage } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { dialogs } from '../../lib/dialogs.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { navigate } from '../../lib/router.svelte';
  import { href } from '../../lib/router';
  import { slugify } from '../../lib/markdown';
  import { NOT_UNDONE } from '../../lib/manage';
  import { formatRelative } from '../../lib/time';
  import Loadable from '../../lib/components/Loadable.svelte';
  import Markdown from '../../lib/components/Markdown.svelte';

  let { project, slug }: { project: ProjectSummary; slug: string | null } = $props();
  const pid = $derived(project.id);

  const pages = new Resource(() => api.projects.wiki(pid));
  // Deleted pages are read while their list is open.
  const deleted = new Resource(() => api.projects.deletedWiki(pid));
  let showDeleted = $state(false);
  $effect(() => {
    void app.memoryTick[pid];
    void pages.reload();
    if (showDeleted) void deleted.reload();
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

  /**
   * A new slug first (a clash, 409, leaves the page as it was), then the text. When the text cannot be
   * saved the rename is taken back, so a failed save changes nothing.
   */
  async function saveEdit(from: string, input: { slug: string; title: string; body_md: string }): Promise<WikiPage> {
    const renamed = input.slug !== from;
    const slugNow = renamed ? (await api.projects.renameWiki(pid, from, input.slug)).slug : from;
    try {
      return await api.projects.updateWiki(pid, slugNow, { title: input.title, body_md: input.body_md });
    } catch (e) {
      if (!renamed) throw e;
      try {
        await api.projects.renameWiki(pid, slugNow, from);
      } catch (back) {
        throw new Error(`${errorMessage(e)}. The page kept its new address ${slugNow}: ${errorMessage(back)}`);
      }
      throw e;
    }
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
      let saved: WikiPage;
      if (mode === 'edit' && current) {
        saved = await saveEdit(current.slug, input);
      } else {
        saved = await api.projects.createWiki(pid, input);
      }
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
    const sure = await dialogs.confirm({
      title: 'Delete wiki page?',
      body: `"${p.title}" is deleted on every synced machine and no longer given to agents. Restore it from Deleted pages to bring it back.`,
      confirm: 'Delete',
      danger: true,
    });
    if (!sure) return;
    const ok = await app.act(async () => {
      await api.projects.deleteWiki(pid, p.slug);
      return true;
    });
    if (!ok) return;
    pages.data = (pages.data ?? []).filter((x) => x.id !== p.id);
    if (showDeleted) void deleted.reload();
    navigate(href.project(pid, 'wiki'));
    app.toast(`Deleted "${p.title}"`, 'info', { label: 'Undo', run: () => void restore(p, false) });
  }

  async function restore(p: WikiPage, undoable = true): Promise<void> {
    const back = await app.act(() => api.projects.restoreWiki(pid, p.slug));
    if (!back) return;
    pages.data = [...(pages.data ?? []).filter((x) => x.id !== back.id), back];
    deleted.data = (deleted.data ?? []).filter((x) => x.id !== back.id);
    app.toast(`Restored "${back.title}"`, 'info', undoable ? { label: 'Undo', run: () => void undoRestore(back) } : undefined);
  }

  /** Undo of a restore: delete it again unless it was edited since. */
  async function undoRestore(p: WikiPage): Promise<void> {
    const now = (pages.data ?? []).find((x) => x.id === p.id);
    if (!now || now.updated_at !== p.updated_at) {
      app.toast(NOT_UNDONE, 'info');
      return;
    }
    const ok = await app.act(async () => {
      await api.projects.deleteWiki(pid, p.slug);
      return true;
    });
    if (!ok) return;
    pages.data = (pages.data ?? []).filter((x) => x.id !== p.id);
    if (showDeleted) void deleted.reload();
    if (slug === p.slug) navigate(href.project(pid, 'wiki'));
  }
</script>

<div class="wiki">
  <aside class="card side">
    <div class="row side-head">
      <h2 class="h">Pages</h2>
      <span class="spacer"></span>
      {#if app.control}<button type="button" class="icon-btn sm" aria-label="New page" title="New page" onclick={startNew}><Plus size={15} /></button>{/if}
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
    <details class="deleted" bind:open={showDeleted}>
      <summary class="small muted">Deleted pages</summary>
      <Loadable
        loading={deleted.loading}
        error={deleted.error}
        empty={(deleted.data ?? []).length === 0}
        emptyText="No deleted pages."
        onretry={() => deleted.load()}
      >
        <ul class="pages" aria-label="Deleted pages">
          {#each deleted.data ?? [] as p (p.id)}
            <li class="row gone">
              <span class="ellipsis grow-name" title={p.slug}>{p.title}</span>
              <span class="faint small">{formatRelative(p.updated_at)}</span>
              {#if app.control}
                <button type="button" class="icon-btn sm" aria-label="Restore {p.title}" title="Restore" onclick={() => restore(p)}
                  ><RotateCcw size={14} /></button
                >
              {/if}
            </li>
          {/each}
        </ul>
      </Loadable>
    </details>
  </aside>

  <section class="card content">
    {#if mode !== 'view' && app.control}
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
              pattern="[a-z0-9\-]+"
              title="Lowercase letters, digits and dashes"
            />
            {#if mode === 'edit' && current && pageSlug !== current.slug}
              <span class="hint">Changes the page's address. Links to the old address written in text are not changed.</span>
            {/if}
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
        {#if app.control}
          <button type="button" class="btn sm" onclick={() => startEdit(current)}><Pencil size={14} aria-hidden="true" />Edit</button>
          <button type="button" class="btn sm ghost" onclick={() => remove(current)}><Trash size={14} aria-hidden="true" />Delete</button>
        {/if}
      </div>
      <Markdown source={current.body_md || '_This page is empty._'} />
    {:else if slug && pages.data}
      <p class="muted">No page named <code>{slug}</code>.</p>
      {#if app.control}<button type="button" class="btn" onclick={startNew}>Create a page</button>{/if}
    {:else}
      <div class="intro">
        <h2 class="h">Project wiki</h2>
        <p class="muted">Longer-lived docs for this project: setup, architecture notes, runbooks. Agents can read them through blirp's memory tools.</p>
        {#if app.control}<button type="button" class="btn primary" onclick={startNew}><Plus size={14} aria-hidden="true" />New page</button>{/if}
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
  .deleted {
    margin-top: 10px;
    border-top: 1px solid var(--border);
    padding-top: 8px;
  }
  .deleted summary {
    cursor: pointer;
  }
  .gone {
    gap: 6px;
    padding: 4px 8px;
  }
  .grow-name {
    flex: 1;
    min-width: 0;
    color: var(--text-2);
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
