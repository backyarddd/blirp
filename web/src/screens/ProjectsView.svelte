<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import { api, errorMessage } from '../lib/api/client';
  import { app } from '../lib/app.svelte';
  import { navigate } from '../lib/router.svelte';
  import { href } from '../lib/router';
  import { formatRelative } from '../lib/time';
  import { projectPlace } from '../lib/status';
  import type { ProjectSummary } from '../lib/api/types.gen';
  import Loadable from '../lib/components/Loadable.svelte';
  import Modal from '../lib/components/Modal.svelte';
  import ProjectBadge from '../lib/components/ProjectBadge.svelte';

  let adding = $state(false);
  let path = $state('');
  let name = $state('');
  let brief = $state('');
  let saving = $state(false);
  let formError: string | null = $state(null);

  const projects = $derived(
    [...app.realProjects].sort((a, b) => (b.last_activity_at ?? b.updated_at) - (a.last_activity_at ?? a.updated_at)),
  );

  const machineCount = (p: ProjectSummary): number => new Set(p.paths.map((x) => x.machine_id)).size;

  function openAdd(): void {
    path = '';
    name = '';
    brief = '';
    formError = null;
    adding = true;
  }

  async function add(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const p = path.trim();
    const n = name.trim();
    if (!p && !n) {
      formError = 'Enter a name, a folder, or both.';
      return;
    }
    saving = true;
    formError = null;
    try {
      const project = await api.projects.create({
        ...(p ? { path: p } : {}),
        ...(n ? { name: n } : {}),
        ...(brief.trim() ? { brief: brief.trim() } : {}),
      });
      app.upsertProject(project);
      adding = false;
      navigate(href.project(project.id));
    } catch (err) {
      formError = errorMessage(err);
    } finally {
      saving = false;
    }
  }
</script>

<div class="page">
  <div class="page-inner">
    <div class="row head">
      <div>
        <h1 class="page-title">Projects</h1>
        <p class="muted sub">A project is a folder, several, or none at all. Git is optional.</p>
      </div>
      <span class="spacer"></span>
      {#if app.control}
        <button type="button" class="btn primary" onclick={openAdd}><Plus size={16} aria-hidden="true" />New project</button>
      {/if}
    </div>

    <Loadable
      loading={!app.projectsLoaded}
      error={app.projectsError}
      empty={projects.length === 0}
      emptyText="No projects yet. Create one, or just start an agent in a project folder and blirp picks it up."
      onretry={() => app.refreshProjects()}
    >
      <ul class="cards">
        {#each projects as p (p.id)}
          <li>
            <a class="pcard card" href={href.project(p.id)}>
              <div class="row">
                <strong class="name ellipsis">{p.name}</strong>
                <span class="spacer"></span>
                <ProjectBadge project={p} />
              </div>
              <ul class="paths">
                {#each p.paths as path (path.machine_id + path.path)}
                  <li class="mono ellipsis" title={path.path}>{path.path}{path.local ? '' : ' (other machine)'}</li>
                {/each}
                {#if projectPlace(p)}<li class="faint">{projectPlace(p)}</li>{/if}
              </ul>
              <div class="row wrap foot small muted">
                <span>{p.session_count} {p.session_count === 1 ? 'session' : 'sessions'}</span>
                {#if p.live_session_count > 0}<span class="badge accent">{p.live_session_count} live</span>{/if}
                {#if machineCount(p) > 1}<span>· {machineCount(p)} machines</span>{/if}
                <span class="spacer"></span>
                <span>{p.last_activity_at ? formatRelative(p.last_activity_at) : 'No activity yet'}</span>
              </div>
            </a>
          </li>
        {/each}
      </ul>
    </Loadable>
  </div>
</div>

<Modal open={adding} title="New project" onclose={() => (adding = false)}>
  <form id="new-project" onsubmit={add}>
    <label class="field">
      <span>Name</span>
      <input class="input" bind:value={name} placeholder={path.trim() ? 'Defaults to the folder name' : 'What is this project?'} maxlength="200" />
    </label>
    <label class="field">
      <span>Folder <span class="faint">(optional)</span></span>
      <input class="input mono" bind:value={path} placeholder="C:\Users\you\Documents\project" spellcheck="false" />
      <span class="hint">
        An absolute path on this machine, with or without git. Leave it empty for a project that lives elsewhere (for
        example in a design tool reached through an MCP server): its sessions start in a private blirp workspace folder.
      </span>
    </label>
    <label class="field">
      <span>Brief <span class="faint">(optional)</span></span>
      <textarea class="textarea" rows="3" bind:value={brief} placeholder="What it is, where its work lives, current priorities"></textarea>
    </label>
    {#if formError}<p class="err" role="alert">{formError}</p>{/if}
  </form>
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (adding = false)}>Cancel</button>
    <button type="submit" form="new-project" class="btn primary" disabled={saving}>{saving ? 'Creating…' : 'Create project'}</button>
  {/snippet}
</Modal>

<style>
  .head {
    margin-bottom: 18px;
    align-items: flex-end;
  }
  .sub {
    margin: 0;
  }
  .cards {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(min(320px, 100%), 1fr));
    gap: 14px;
  }
  .pcard {
    display: grid;
    gap: 10px;
    padding: 16px;
    text-decoration: none;
    height: 100%;
  }
  .pcard:hover {
    border-color: var(--border-strong);
  }
  .name {
    font-size: 15px;
  }
  .paths {
    list-style: none;
    margin: 0;
    padding: 0;
    font-size: 12px;
    color: var(--text-2);
    display: grid;
    gap: 2px;
  }
  .foot {
    border-top: 1px solid var(--border);
    padding-top: 10px;
  }
  .err {
    color: var(--danger);
    margin: 0;
  }
</style>
