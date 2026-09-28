<script lang="ts">
  import Pencil from '@lucide/svelte/icons/pencil';
  import Plus from '@lucide/svelte/icons/plus';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import { app } from '../lib/app.svelte';
  import { href, type ProjectTab } from '../lib/router';
  import { folderActions, projectActions } from '../lib/actions';
  import { actionEnv, projectOps, renameProject } from '../lib/manage';
  import { contextmenu } from '../lib/contextmenu';
  import Menu from '../lib/components/Menu.svelte';
  import ProjectBadge from '../lib/components/ProjectBadge.svelte';
  import Overview from './project/Overview.svelte';
  import Sessions from './project/Sessions.svelte';
  import Memory from './project/Memory.svelte';
  import Wiki from './project/Wiki.svelte';
  import Resources from './project/Resources.svelte';
  import Files from './project/Files.svelte';
  import HubFiles from './project/HubFiles.svelte';
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
    { id: 'hub-files', label: 'Files on hub' },
    { id: 'git', label: 'Git' },
  ];
  // Files and git read this machine's folders (or its blirp workspace); git only exists for folders that are repos.
  const hasLocal = $derived((project?.paths.some((p) => p.local) ?? false) || (project?.workspace ?? null) !== null);
  const hasGit = $derived(project?.paths.some((p) => p.local && p.is_git) ?? false);
  // Files on hub: only when this machine is a hub or paired with one.
  const synced = $derived((app.sync?.role ?? app.health?.role ?? 'standalone') !== 'standalone');
  const tabs = $derived(
    TABS.filter(
      (t) =>
        (t.id !== 'git' || hasGit) &&
        (t.id !== 'files' || hasLocal) &&
        (t.id !== 'hub-files' || synced) &&
        // Portal devices without the Files permission read no project files.
        (app.canFiles || !['files', 'hub-files', 'git'].includes(t.id)),
    ),
  );

  let renaming = $state(false);
  let newName = $state('');

  async function rename(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (!project) return;
    if (await renameProject(project, newName)) renaming = false;
  }

  const actions = $derived(project ? projectActions(project, actionEnv(), projectOps) : []);
</script>

<div class="page">
  <div class="page-inner">
    {#if !project}
      {#if !app.projectsLoaded}
        <p class="muted" role="status">Loading project…</p>
      {:else}
        <div class="card panel-pad">
          <h1 class="page-title">Project not found</h1>
          <p class="muted">It may be in the Trash, or merged into another project.</p>
          <div class="row">
            <a class="btn" href={href.projects()}>All projects</a>
            <a class="btn" href={href.trash()}>Trash</a>
          </div>
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
        <Menu items={actions} label="Project actions" title="More actions" triggerClass="btn ghost sm" align="right"
          ><Ellipsis size={15} aria-hidden="true" />Actions</Menu
        >
        {#if app.control}
          <button type="button" class="btn primary" onclick={() => app.openNewSession(project.id)}><Plus size={16} aria-hidden="true" />New session</button>
        {/if}
      </div>
      <ul class="paths small muted" aria-label="Folders">
        {#each project.paths as p (p.machine_id + p.path)}
          <li class="row" use:contextmenu={{ items: () => (p.local ? folderActions(project, p.path, actionEnv(), projectOps) : []), label: p.path }}>
            <span class="mono ellipsis" title={p.path}>{p.path}{p.git_remote ? `  ·  ${p.git_remote}` : ''}{p.local ? '' : ` (${app.machineName(p.machine_id)})`}</span>
            {#if p.local}
              <Menu
                items={folderActions(project, p.path, actionEnv(), projectOps)}
                label="Actions for folder {p.path}"
                title="Folder actions"
                triggerClass="icon-btn sm"><Ellipsis size={13} /></Menu
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
        {:else if tab === 'hub-files'}
          <HubFiles {project} />
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
