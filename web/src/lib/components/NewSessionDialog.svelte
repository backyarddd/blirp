<script lang="ts">
  import { untrack } from 'svelte';
  import { navigate } from '../router.svelte';
  import { href } from '../router';
  import { api, errorMessage } from '../api/client';
  import type { LaunchSession, Machine } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { agentLabel } from '../status';
  import Modal from './Modal.svelte';

  let source: 'project' | 'path' = $state('project');
  let projectId = $state('');
  let path = $state('');
  let agent = $state('');
  let prompt = $state('');
  let worktree = $state(false);
  let machine = $state('');
  let machines: Machine[] = $state.raw([]);
  let submitting = $state(false);
  let formError: string | null = $state(null);

  const open = $derived(app.newSession.open);
  const synced = $derived((app.sync?.role ?? app.health?.role ?? 'standalone') !== 'standalone');
  // Agents are detected on this machine; another machine may have others installed.
  const remote = $derived(machine !== '');
  const project = $derived(app.projectById.get(projectId));
  const showWorktree = $derived(source === 'project' && project?.is_git === true);
  const installed = $derived(app.agents.filter((a) => a.installed));

  // Reset the form each time the dialog opens; untracked so later store updates don't wipe input.
  let opens = 0;
  $effect(() => {
    if (open) untrack(() => void init(++opens));
  });

  async function init(token: number): Promise<void> {
    formError = null;
    prompt = '';
    const preset = app.newSession.projectId;
    source = app.projects.length === 0 ? 'path' : 'project';
    projectId = preset ?? app.projects[0]?.id ?? '';
    path = '';
    machine = '';
    agent = '';
    worktree = false;
    const [settings] = await Promise.all([
      api.settings.get().catch((e: unknown) => {
        console.warn('blirp: settings unavailable for new session defaults', e);
        return null;
      }),
      app.refreshAgents(),
    ]);
    if (token !== opens) return;
    // Defaults arrive after the dialog is usable; never overwrite a choice made meanwhile.
    const def = settings?.config.agents.default;
    if (!agent) agent = installed.find((a) => a.id === def)?.id ?? installed[0]?.id ?? '';
    if (!worktree) worktree = settings?.config.sessions.worktree_default ?? false;
    if (synced) {
      try {
        const self = app.health?.machine.id;
        machines = (await api.machines.list()).filter((m) => !m.revoked && m.id !== self);
      } catch (e) {
        formError = `Could not load machines: ${errorMessage(e)}`;
      }
    }
  }

  function close(): void {
    app.newSession = { open: false, projectId: null };
  }

  async function submit(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    formError = null;
    if (!agent) {
      formError = 'Pick an installed agent.';
      return;
    }
    const req: LaunchSession = { agent };
    if (source === 'project') {
      if (!projectId) {
        formError = 'Pick a project.';
        return;
      }
      req.project_id = projectId;
      if (showWorktree && worktree) req.worktree = true;
    } else {
      const p = path.trim();
      if (!p) {
        formError = 'Enter a folder path.';
        return;
      }
      req.cwd = p;
    }
    if (prompt.trim()) req.prompt = prompt.trim();
    if (synced && machine) req.machine = machine;
    submitting = true;
    try {
      const s = await api.sessions.launch(req);
      app.upsertSession(s);
      close();
      navigate(href.sessions(s.id));
    } catch (err) {
      app.noteForbidden(err);
      formError = errorMessage(err);
    } finally {
      submitting = false;
    }
  }
</script>

<Modal {open} title="New session" onclose={close}>
  <form id="new-session" onsubmit={submit}>
    <div class="pills" role="radiogroup" aria-label="Where to run">
      <button
        type="button"
        class="pill"
        role="radio"
        aria-checked={source === 'project'}
        class:active={source === 'project'}
        disabled={app.projects.length === 0}
        onclick={() => (source = 'project')}>Project</button
      >
      <button
        type="button"
        class="pill"
        role="radio"
        aria-checked={source === 'path'}
        class:active={source === 'path'}
        onclick={() => (source = 'path')}>Folder path</button
      >
    </div>

    {#if source === 'project'}
      <label class="field top">
        <span>Project</span>
        <select class="select" bind:value={projectId} required>
          {#each app.projects as p (p.id)}
            <option value={p.id}>{p.name}{p.is_git ? '' : ' (folder)'}</option>
          {/each}
        </select>
        {#if project?.paths[0]}<span class="hint mono ellipsis">{project.paths[0].path}</span>{/if}
      </label>
    {:else}
      <label class="field top">
        <span>Folder</span>
        <input class="input mono" bind:value={path} placeholder="C:\Users\you\Documents\project" required spellcheck="false" />
        <span class="hint">Any folder works; git is optional. It becomes a project automatically.</span>
      </label>
    {/if}

    <label class="field">
      <span>Agent</span>
      <select class="select" bind:value={agent} required>
        {#if app.agents.length === 0}
          <option value="" disabled>{app.agentsError ? 'Could not load agents' : app.agentsLoaded ? 'No agents detected' : 'Detecting agents…'}</option>
        {/if}
        {#each app.agents as a (a.id)}
          <option value={a.id} disabled={!a.installed && !remote}>
            {a.display_name || agentLabel(a.id)}{a.version && !remote ? ` ${a.version}` : ''}{a.installed || remote ? '' : ' (not installed)'}
          </option>
        {/each}
      </select>
    </label>

    <label class="field">
      <span>First prompt <span class="faint">(optional)</span></span>
      <textarea class="textarea" rows="3" bind:value={prompt} placeholder="What should the agent work on?"></textarea>
    </label>

    {#if showWorktree}
      <label class="check field">
        <input type="checkbox" bind:checked={worktree} />
        <span>Run in a new git worktree <span class="faint">(isolated branch blirp/&lt;name&gt;)</span></span>
      </label>
    {/if}

    {#if synced}
      <label class="field">
        <span>Machine</span>
        <select class="select" bind:value={machine}>
          <option value="">This machine</option>
          {#each machines as m (m.id)}
            <option value={m.id}>{m.name} ({m.os})</option>
          {/each}
        </select>
        {#if remote}<span class="hint">Runs on that machine through the hub; its terminal streams here.</span>{/if}
      </label>
    {/if}

    {#if formError}<p class="error" role="alert">{formError}</p>{/if}
  </form>

  {#snippet footer()}
    <button type="button" class="btn" onclick={close}>Cancel</button>
    <button type="submit" form="new-session" class="btn primary" disabled={submitting || !agent}>
      {submitting ? 'Starting…' : 'Start session'}
    </button>
  {/snippet}
</Modal>

<style>
  .top {
    margin-top: 14px;
  }
  .error {
    color: var(--danger);
    margin: 0;
  }
</style>
