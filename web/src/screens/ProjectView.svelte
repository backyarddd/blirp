<script lang="ts">
  import Pencil from '@lucide/svelte/icons/pencil';
  import Plus from '@lucide/svelte/icons/plus';
  import Trash from '@lucide/svelte/icons/trash-2';
  import { api } from '../lib/api/client';
  import { app } from '../lib/app.svelte';
  import { navigate } from '../lib/router.svelte';
  import { href, type ProjectTab } from '../lib/router';
  import ProjectBadge from '../lib/components/ProjectBadge.svelte';
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
  // Files and git read this machine's folders; git only exists for folders that are repos.
  const hasLocal = $derived(project?.paths.some((p) => p.local) ?? false);
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

  async function remove(): Promise<void> {
    if (!project) return;
    if (!confirm(`Remove "${project.name}" from blirp? Files on disk are not touched; its memory is hidden.`)) return;
    const ok = await app.act(async () => {
      await api.projects.remove(project.id);
      return true;
    });
    if (ok) {
      app.removeProject(project.id);
      navigate(href.projects());
    }
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
        <span class="spacer"></span>
        <button type="button" class="btn ghost sm" onclick={remove}><Trash size={14} aria-hidden="true" />Remove</button>
        <button type="button" class="btn primary" onclick={() => app.openNewSession(project.id)}><Plus size={16} aria-hidden="true" />New session</button>
      </div>
      <ul class="paths small muted">
        {#each project.paths as p (p.machine_id + p.path)}
          <li class="mono ellipsis" title={p.path}>{p.path}{p.git_remote ? `  ·  ${p.git_remote}` : ''}</li>
        {/each}
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
  </div>
</div>

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
</style>
