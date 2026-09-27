<script lang="ts">
  import Pencil from '@lucide/svelte/icons/pencil';
  import Plus from '@lucide/svelte/icons/plus';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Merge from '@lucide/svelte/icons/git-merge';
  import X from '@lucide/svelte/icons/x';
  import { api } from '../lib/api/client';
  import { app } from '../lib/app.svelte';
  import { navigate } from '../lib/router.svelte';
  import { href, type ProjectTab } from '../lib/router';
  import ProjectBadge from '../lib/components/ProjectBadge.svelte';
  import Modal from '../lib/components/Modal.svelte';
  import Overview from './project/Overview.svelte';
  import Sessions from './project/Sessions.svelte';
  import Memory from './project/Memory.svelte';
  import Wiki from './project/Wiki.svelte';
  import Resources from './project/Resources.svelte';
  import Files from './project/Files.svelte';
  import Git from './project/Git.svelte';

  let { projectId, tab, sub }: { projectId: string; tab: ProjectTab; sub: string | null } = $props();

  const project = $derived(app.projectById.get(projectId));

  const TABS: { id: ProjectTab; label: string }[] = [
    { id: 'overview', label: 'Overview' },
    { id: 'sessions', label: 'Sessions' },
    { id: 'memory', label: 'Memory' },
    { id: 'wiki', label: 'Wiki' },
    { id: 'resources', label: 'Resources' },
    { id: 'files', label: 'Files' },
    { id: 'git', label: 'Git' },
  ];
  // Files and git read this machine's folders (or its blirp workspace); git only exists for folders that are repos.
  const hasLocal = $derived((project?.paths.some((p) => p.local) ?? false) || (project?.workspace ?? null) !== null);
  const hasGit = $derived(project?.paths.some((p) => p.local && p.is_git) ?? false);
  const tabs = $derived(TABS.filter((t) => (t.id !== 'git' || hasGit) && (t.id !== 'files' || hasLocal)));

  let renaming = $state(false);
  let newName = $state('');

  async function rename(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const name = newName.trim();
    if (!name || !project) return;
    const p = await app.act(() => api.projects.rename(project.id, name));
    if (p) {
      app.upsertProject(p);
      renaming = false;
    }
  }

  async function removeFolder(path: string): Promise<void> {
    if (!project) return;
    const last = project.paths.length === 1;
    const msg =
      `Remove ${path} from "${project.name}"? The folder itself is not touched, and the project keeps its sessions and memory.` +
      (last ? ' With no folder left, its new sessions start in a blirp workspace.' : '');
    if (!confirm(msg)) return;
    const p = await app.act(() => api.projects.removeFolder(project.id, path), 'Folder removed');
    if (p) app.upsertProject(p);
  }

  async function remove(): Promise<void> {
    if (!project) return;
    const msg =
      `Delete "${project.name}" from blirp? It is hidden and its folders are unregistered on every synced machine. ` +
      'Files on disk are not touched, and its sessions and memory stay in the database. ' +
      'A new session in one of its folders starts a new project.';
    if (!confirm(msg)) return;
    const ok = await app.act(async () => {
      await api.projects.remove(project.id);
      return true;
    });
    if (ok) {
      app.removeProject(project.id);
      navigate(href.projects());
    }
  }

  let merging = $state(false);
  let mergeInto = $state('');
  let mergeBusy = $state(false);
  const mergeTargets = $derived(
    app.realProjects.filter((p) => p.id !== projectId).sort((a, b) => a.name.localeCompare(b.name)),
  );
  const mergeTarget = $derived(app.projectById.get(mergeInto));

  function openMerge(): void {
    mergeInto = '';
    merging = true;
  }

  async function merge(): Promise<void> {
    const from = project;
    const into = mergeTarget;
    if (!from || !into) return;
    mergeBusy = true;
    const merged = await app.act(() => api.projects.merge(from.id, into.id), `Merged "${from.name}" into "${into.name}"`);
    mergeBusy = false;
    if (!merged) return;
    merging = false;
    app.upsertProject(merged);
    app.removeProject(from.id);
    // Moved sessions keep their ids; the daemon only reports the two projects.
    void app.refreshSessions();
    navigate(href.project(merged.id));
  }
</script>

<div class="page">
  <div class="page-inner">
    {#if !project}
      {#if !app.projectsLoaded}
        <p class="muted" role="status">Loading project…</p>
      {:else}
        <div class="card panel-pad">
          <h1 class="page-title">Project not found</h1>
          <p class="muted">It may have been removed or merged into another project.</p>
          <a class="btn" href={href.projects()}>All projects</a>
        </div>
      {/if}
    {:else}
      {#if project.chats}
        <div class="card panel-pad">
          <h1 class="page-title">Chats</h1>
          <p class="muted">
            Sessions that belong to no project are chats. They are listed under Chats in Sessions, searchable and summarized, but get
            no project memory. Move one into a project from its session toolbar.
          </p>
          <a class="btn" href={href.sessions()}>Sessions</a>
        </div>
      {:else}
      <nav class="crumbs small" aria-label="Breadcrumb"><a href={href.projects()}>Projects</a> / <span aria-current="page">{project.name}</span></nav>
      <div class="row wrap head">
        {#if renaming}
          <form class="row" onsubmit={rename}>
            <input class="input" bind:value={newName} aria-label="Project name" required />
            <button type="submit" class="btn sm primary">Save</button>
            <button type="button" class="btn sm" onclick={() => (renaming = false)}>Cancel</button>
          </form>
        {:else}
          <h1 class="page-title ellipsis">{project.name}</h1>
          <ProjectBadge {project} />
          {#if app.control}
          <button
            type="button"
            class="icon-btn sm"
            aria-label="Rename project"
            title="Rename"
            onclick={() => {
              newName = project.name;
              renaming = true;
            }}><Pencil size={14} /></button
          >
          {/if}
        {/if}
        <span class="spacer"></span>
        {#if app.control}
          <button type="button" class="btn ghost sm" onclick={openMerge} disabled={mergeTargets.length === 0}
            ><Merge size={14} aria-hidden="true" />Merge into…</button
          >
          <button type="button" class="btn ghost sm" onclick={remove}><Trash size={14} aria-hidden="true" />Delete</button>
          <button type="button" class="btn primary" onclick={() => app.openNewSession(project.id)}><Plus size={16} aria-hidden="true" />New session</button>
        {/if}
      </div>
      <ul class="paths small muted" aria-label="Folders">
        {#each project.paths as p (p.machine_id + p.path)}
          <li class="row">
            <span class="mono ellipsis" title={p.path}>{p.path}{p.git_remote ? `  ·  ${p.git_remote}` : ''}{p.local ? '' : ` (${app.machineName(p.machine_id)})`}</span>
            {#if p.local && app.control}
              <button type="button" class="icon-btn sm" aria-label="Remove folder {p.path}" title="Remove folder from project" onclick={() => removeFolder(p.path)}
                ><X size={13} /></button
              >
            {/if}
          </li>
        {/each}
        {#if project.workspace !== null}
          <li class="row" data-testid="workspace">
            <span class="badge">blirp workspace</span>
            <span class="mono ellipsis" title={project.workspace}>{project.workspace}</span>
          </li>
          <li class="faint">
            No folder: sessions start in this machine's blirp workspace (created with the first one). Put agent settings there, such
            as a <code>.mcp.json</code>.
          </li>
        {/if}
      </ul>

      <nav class="tabs" aria-label="Project sections">
        {#each tabs as t (t.id)}
          <a class="tab" href={href.project(project.id, t.id)} aria-current={t.id === tab ? 'page' : undefined}>{t.label}</a>
        {/each}
      </nav>

      {#key project.id}
        {#if tab === 'overview'}
          <Overview {project} />
        {:else if tab === 'sessions'}
          <Sessions {project} />
        {:else if tab === 'memory'}
          <Memory {project} />
        {:else if tab === 'wiki'}
          <Wiki {project} slug={sub} />
        {:else if tab === 'resources'}
          <Resources {project} />
        {:else if tab === 'files'}
          {#if hasLocal}
            <Files {project} />
          {:else}
            <p class="muted">This project has no folder on this machine.</p>
          {/if}
        {:else if tab === 'git'}
          {#if hasGit}
            <Git {project} />
          {:else}
            <p class="muted">This project is a plain folder, so there is no git view.</p>
          {/if}
        {/if}
      {/key}
      {/if}
    {/if}
  </div>
</div>

<Modal open={merging && project !== undefined} title="Merge project" onclose={() => (merging = false)}>
  {#if project}
    <form id="merge-project" onsubmit={(e) => (e.preventDefault(), void merge())}>
      <label class="field">
        <span>Merge "{project.name}" into</span>
        <select class="select" bind:value={mergeInto} required>
          <option value="" disabled>Pick a project</option>
          {#each mergeTargets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
        </select>
      </label>
      <p class="small">
        Everything in "{project.name}" moves to {mergeTarget ? `"${mergeTarget.name}"` : 'the project you pick'}: its folders, sessions,
        records, wiki pages (a clashing page name gets a number added), resources and pending suggestions. Its brief moves only when
        the target has none; otherwise the target's brief is kept. "{project.name}" is then deleted.
      </p>
      <p class="small muted">The change syncs to every paired machine and cannot be undone. Files on disk are not touched.</p>
    </form>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (merging = false)}>Cancel</button>
    <button type="submit" form="merge-project" class="btn primary" disabled={!mergeTarget || mergeBusy}>{mergeBusy ? 'Merging…' : 'Merge'}</button>
  {/snippet}
</Modal>

<style>
  .crumbs a {
    color: var(--text-2);
    text-decoration: none;
  }
  .crumbs {
    color: var(--text-3);
    margin-bottom: 6px;
  }
  .head {
    gap: 10px;
  }
  .head .page-title {
    margin: 0;
  }
  .paths {
    list-style: none;
    padding: 0;
    margin: 6px 0 16px;
  }
  .paths li {
    gap: 6px;
    min-width: 0;
  }
  .paths .mono {
    min-width: 0;
  }
</style>
