<script lang="ts">
  import { untrack } from 'svelte';
  import Cloud from '@lucide/svelte/icons/cloud';
  import Monitor from '@lucide/svelte/icons/monitor';
  import Laptop from '@lucide/svelte/icons/laptop';
  import { navigate } from '../router.svelte';
  import { href } from '../router';
  import { api, errorMessage } from '../api/client';
  import type { AgentInfo, CloneJob, LaunchSession } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { agentLabel } from '../status';
  import { folderOn, recentFolders } from '../machines';
  import Modal from './Modal.svelte';
  import FolderPicker from './FolderPicker.svelte';

  let source: 'project' | 'path' = $state('project');
  let projectId = $state('');
  let path = $state('');
  let agent = $state('');
  let prompt = $state('');
  let worktree = $state(false);
  /** Machine to run on; '' is this machine. */
  let target = $state('');
  let submitting = $state(false);
  let formError: string | null = $state(null);
  let defaultAgent: string | undefined;

  const open = $derived(app.newSession.open);
  const synced = $derived((app.sync?.role ?? app.health?.role ?? 'standalone') !== 'standalone');
  const remote = $derived(target !== '');
  const targetName = $derived(remote ? app.machineName(target) : (app.health?.machine.name ?? 'this machine'));
  const cloud = $derived(remote && app.remote(target)?.cloud === true);
  // The hub first (cloud sessions), then the other paired machines.
  const others = $derived(
    app.machines
      .filter((m) => m.id !== app.selfId)
      .sort((a, b) => Number(b.id === app.cloudId) - Number(a.id === app.cloudId) || a.name.localeCompare(b.name)),
  );
  const project = $derived(app.projectById.get(projectId));
  const showWorktree = $derived(source === 'project' && project?.is_git === true);

  // Agents are detected on the machine that runs the session: another machine is asked through the hub.
  let remoteAgents: AgentInfo[] | null = $state.raw(null);
  let remoteAgentsError: string | null = $state(null);
  const agentList = $derived(remote ? (remoteAgents ?? []) : app.agents);
  const installed = $derived(agentList.filter((a) => a.installed));
  const chosen = $derived(agentList.find((a) => a.id === agent));
  const notLoggedIn = $derived(chosen?.auth?.logged_in === false);

  // Where the project lives on the target machine; null when it has no folder there yet.
  const targetFolder = $derived(remote ? folderOn(project, target) : null);
  const cloneable = $derived(project?.paths.some((p) => p.local && p.git_remote !== null) === true);
  const recents = $derived(remote ? recentFolders(app.sessions, app.projects, target) : []);

  let browsing = $state(false);
  let clone: CloneJob | null = $state.raw(null);
  let cloneError: string | null = $state(null);
  /** Folder on the target to clone into; empty for its `~/blirp`. */
  let cloneParent = $state('');
  let pickingParent = $state(false);
  let clonePoll: ReturnType<typeof setTimeout> | undefined;

  // Reset the form each time the dialog opens; untracked so later store updates don't wipe input.
  let opens = 0;
  $effect(() => {
    if (open) untrack(() => void init(++opens));
  });

  function resetTargetState(): void {
    browsing = false;
    pickingParent = false;
    clone = null;
    cloneError = null;
    cloneParent = '';
    clearTimeout(clonePoll);
  }

  async function init(token: number): Promise<void> {
    formError = null;
    prompt = '';
    const preset = app.newSession.projectId;
    source = app.projects.length === 0 ? 'path' : 'project';
    projectId = preset ?? app.projects[0]?.id ?? '';
    path = '';
    target = '';
    agent = '';
    worktree = false;
    remoteAgents = null;
    resetTargetState();
    const [settings] = await Promise.all([
      api.settings.get().catch((e: unknown) => {
        console.warn('blirp: settings unavailable for new session defaults', e);
        return null;
      }),
      app.refreshAgents(),
      synced ? app.refreshMachines() : Promise.resolve(),
    ]);
    if (token !== opens) return;
    // Defaults arrive after the dialog is usable; never overwrite a choice made meanwhile.
    defaultAgent = settings?.config.agents.default;
    if (!agent) agent = installed.find((a) => a.id === defaultAgent)?.id ?? installed[0]?.id ?? '';
    if (!worktree) worktree = settings?.config.sessions.worktree_default ?? false;
  }

  let agentsSeq = 0;
  async function pickTarget(id: string): Promise<void> {
    if (id === target) return;
    target = id;
    resetTargetState();
    remoteAgents = null;
    remoteAgentsError = null;
    const mine = ++agentsSeq;
    if (id === '') {
      agent = installed.find((a) => a.id === agent)?.id ?? installed.find((a) => a.id === defaultAgent)?.id ?? installed[0]?.id ?? '';
      return;
    }
    // A project without a folder there and nothing to clone: start from a folder on that machine.
    if (source === 'project' && folderOn(project, id) === null && !cloneable) source = 'path';
    try {
      const list = await api.machines.agents(id);
      if (mine !== agentsSeq) return;
      remoteAgents = list;
      const ok = list.filter((a) => a.installed);
      agent = ok.find((a) => a.id === agent)?.id ?? ok.find((a) => a.id === defaultAgent)?.id ?? ok[0]?.id ?? '';
    } catch (e) {
      if (mine !== agentsSeq) return;
      remoteAgentsError = `Could not reach ${app.machineName(id)}: ${errorMessage(e)}`;
    }
  }

  async function startClone(): Promise<void> {
    if (!project) return;
    cloneError = null;
    try {
      const req = cloneParent ? { project_id: project.id, parent: cloneParent } : { project_id: project.id };
      clone = await api.machines.clone(target, req);
      pollClone(clone.id, target);
    } catch (e) {
      app.noteForbidden(e);
      cloneError = errorMessage(e);
    }
  }

  function pollClone(job: string, machine: string): void {
    clonePoll = setTimeout(async () => {
      if (machine !== target || clone?.id !== job) return;
      try {
        const j = await api.machines.cloneJob(machine, job);
        if (machine !== target || clone?.id !== job) return;
        clone = j;
        if (j.state === 'running') return pollClone(job, machine);
        if (j.state === 'done') {
          // The launch in that folder joins the same project (matched by git remote).
          source = 'path';
          path = j.dest;
          app.toast(`Cloned into ${j.dest} on ${app.machineName(machine)}`, 'info');
        } else {
          cloneError = j.error ?? 'git clone failed';
        }
      } catch (e) {
        cloneError = `Lost track of the clone: ${errorMessage(e)}`;
      }
    }, 1000);
  }

  function close(): void {
    clearTimeout(clonePoll);
    app.newSession = { open: false, projectId: null };
  }

  async function submit(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    formError = null;
    if (!agent) {
      formError = remote ? `Pick an agent installed on ${targetName}.` : 'Pick an installed agent.';
      return;
    }
    const req: LaunchSession = { agent };
    if (source === 'project') {
      if (!projectId) {
        formError = 'Pick a project.';
        return;
      }
      if (remote && targetFolder === null) {
        formError = `${project?.name ?? 'This project'} has no folder on ${targetName} yet. ${cloneable ? `Clone it there first.` : `Pick a folder on ${targetName}.`}`;
        return;
      }
      req.project_id = projectId;
      if (showWorktree && worktree) req.worktree = true;
    } else {
      const p = path.trim();
      if (!p) {
        formError = remote ? `Enter or browse to a folder on ${targetName}.` : 'Enter a folder path.';
        return;
      }
      req.cwd = p;
    }
    if (prompt.trim()) req.prompt = prompt.trim();
    if (remote) req.machine = target;
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
    {#if synced && others.length > 0}
      <div class="field">
        <span id="run-on">Run on</span>
        <div class="targets" role="radiogroup" aria-labelledby="run-on">
          <button type="button" class="target" role="radio" aria-checked={!remote} class:active={!remote} onclick={() => pickTarget('')}>
            <Laptop size={16} aria-hidden="true" />
            <span class="tname">This machine</span>
            <span class="tsub faint ellipsis">{app.health?.machine.name ?? ''}</span>
          </button>
          {#each others as m (m.id)}
            {@const isCloud = m.id === app.cloudId || m.role === 'hub'}
            <button
              type="button"
              class="target"
              class:cloud={isCloud}
              class:active={target === m.id}
              role="radio"
              aria-checked={target === m.id}
              onclick={() => pickTarget(m.id)}
            >
              {#if isCloud}<Cloud size={16} aria-hidden="true" />{:else}<Monitor size={16} aria-hidden="true" />{/if}
              <span class="tname ellipsis">{isCloud ? `Cloud (${m.name})` : m.name}</span>
              <span class="tsub faint ellipsis">{isCloud ? 'keeps running when this PC sleeps' : m.os}</span>
            </button>
          {/each}
        </div>
        {#if cloud}
          <span class="hint">
            Runs on {targetName}, your hub. Close or sleep this computer and the session keeps going; reopen blirp to
            pick it up where it is. {targetName} is kept awake while sessions run.
          </span>
        {:else if remote}
          <span class="hint">Runs on {targetName} through the hub; its terminal streams here.</span>
        {/if}
        {#if remoteAgentsError}<span class="hint warn" role="alert">{remoteAgentsError}</span>{/if}
      </div>
    {/if}

    <div class="pills top" role="radiogroup" aria-label="Where to start">
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
        onclick={() => (source = 'path')}>{remote ? `Folder on ${targetName}` : 'Folder path'}</button
      >
    </div>

    {#if source === 'project'}
      <label class="field top">
        <span>Project</span>
        <select class="select" bind:value={projectId} required onchange={() => resetTargetState()}>
          {#each app.projects as p (p.id)}
            <option value={p.id}>{p.name}{p.is_git ? '' : ' (folder)'}</option>
          {/each}
        </select>
        {#if remote}
          {#if targetFolder}<span class="hint mono ellipsis">{targetFolder} on {targetName}</span>{/if}
        {:else if project?.paths[0]}
          <span class="hint mono ellipsis">{project.paths[0].path}</span>
        {/if}
      </label>
      {#if remote && project && targetFolder === null}
        <div class="notice" data-testid="clone-box">
          {#if cloneable}
            <p>
              <strong>{project.name}</strong> is not on {targetName} yet. Clone it there from its git remote; {targetName}
              uses its own git credentials, nothing is copied from here.
            </p>
            {#if clone?.state === 'running'}
              <p class="mono small" role="status">Cloning into {clone.dest}… {clone.progress ?? ''}</p>
            {:else if clone?.state === 'done'}
              <p class="small" role="status">Cloned into <span class="mono">{clone.dest}</span>.</p>
            {:else}
              <div class="row">
                <button type="button" class="btn sm primary" onclick={startClone} disabled={!app.control}>Clone on {targetName}</button>
                <span class="small faint ellipsis">into <span class="mono">{cloneParent || '~/blirp'}</span></span>
                <button type="button" class="btn sm" onclick={() => (pickingParent = !pickingParent)}>{pickingParent ? 'Cancel' : 'Change…'}</button>
              </div>
              {#if pickingParent}
                <FolderPicker
                  machineId={target}
                  machineName={targetName}
                  pickLabel="Clone into this folder"
                  onpick={(p) => {
                    cloneParent = p;
                    pickingParent = false;
                  }}
                />
              {/if}
            {/if}
            {#if cloneError}<pre class="error small" role="alert">{cloneError}</pre>{/if}
          {:else}
            <p>
              <strong>{project.name}</strong> has no folder on {targetName} and is not a git repository, so it cannot be
              cloned. Pick an existing folder on {targetName} instead.
            </p>
            <button type="button" class="btn sm" onclick={() => (source = 'path')}>Pick a folder on {targetName}</button>
          {/if}
        </div>
      {/if}
    {:else}
      <div class="field top">
        <label class="label" for="ns-path">Folder{remote ? ` on ${targetName}` : ''}</label>
        <div class="row">
          <input
            id="ns-path"
            class="input mono grow"
            bind:value={path}
            placeholder={remote ? '/Users/you/project' : 'C:\\Users\\you\\Documents\\project'}
            required
            spellcheck="false"
          />
          <button type="button" class="btn" onclick={() => (browsing = !browsing)} aria-expanded={browsing}>
            {browsing ? 'Close' : 'Browse…'}
          </button>
        </div>
        {#if browsing}
          <FolderPicker
            machineId={target || (app.selfId ?? '')}
            machineName={targetName}
            start={path.trim()}
            onpick={(p) => {
              path = p;
              browsing = false;
            }}
          />
        {/if}
        {#if recents.length > 0}
          <div class="recents" aria-label="Recent folders on {targetName}">
            <span class="small faint">Recent on {targetName}:</span>
            {#each recents as r (r)}
              <button type="button" class="chip mono" title={r} onclick={() => (path = r)}>{r.split(/[\\/]/).filter(Boolean).pop() ?? r}</button>
            {/each}
          </div>
        {/if}
        <span class="hint">
          {#if remote}
            The folder must exist on {targetName}; git is optional. Tools the agent talks to on that machine (for example
            a desktop app behind an MCP server) must run on {targetName} too.
          {:else}
            Any folder works; git is optional. It becomes a project automatically.
          {/if}
        </span>
      </div>
    {/if}

    <label class="field">
      <span>Agent{remote ? ` on ${targetName}` : ''}</span>
      <select class="select" bind:value={agent} required>
        {#if agentList.length === 0}
          <option value="" disabled>
            {remote
              ? remoteAgentsError
                ? 'Could not load agents'
                : `Detecting agents on ${targetName}…`
              : app.agentsError
                ? 'Could not load agents'
                : app.agentsLoaded
                  ? 'No agents detected'
                  : 'Detecting agents…'}
          </option>
        {/if}
        {#each agentList as a (a.id)}
          <option value={a.id} disabled={!a.installed}>
            {a.display_name || agentLabel(a.id)}{a.version ? ` ${a.version}` : ''}{a.installed ? '' : ' (not installed)'}{a.auth?.logged_in === false ? ' (not logged in)' : ''}
          </option>
        {/each}
      </select>
      {#if notLoggedIn}
        <span class="hint warn" role="alert" data-testid="agent-login-warning">
          {chosen?.display_name || agentLabel(agent)} is not logged in on {targetName} (as seen by its blirp daemon). A daemon
          started over SSH or by a LaunchAgent on a locked Mac cannot read the login keychain. Run
          <code>claude setup-token</code> on any machine with a browser, then on {targetName}
          <code>blirp agents set-token claude</code> and paste the token (or paste it in that machine's Settings &gt; Agents).
        </span>
      {/if}
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

    {#if formError}<p class="error" role="alert">{formError}</p>{/if}
  </form>

  {#snippet footer()}
    <button type="button" class="btn" onclick={close}>Cancel</button>
    <button type="submit" form="new-session" class="btn primary" disabled={submitting || !agent}>
      {submitting ? 'Starting…' : cloud ? `Start on ${targetName}` : 'Start session'}
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
  pre.error {
    white-space: pre-wrap;
    margin: 6px 0 0;
  }
  .targets {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
    gap: 8px;
  }
  .target {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    align-items: center;
    column-gap: 8px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    color: var(--text);
    text-align: left;
    cursor: pointer;
    font: inherit;
  }
  .target :global(svg) {
    grid-row: span 2;
  }
  .target:hover {
    border-color: var(--border-strong);
  }
  .target.cloud {
    border-color: color-mix(in srgb, var(--accent) 45%, var(--border));
  }
  .target.active {
    background: var(--selected);
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-soft);
  }
  .tname {
    font-weight: 600;
  }
  .tsub {
    font-size: 11.5px;
  }
  .hint.warn {
    color: var(--waiting);
  }
  .notice {
    display: grid;
    gap: 8px;
    margin: 4px 0 12px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel-2);
  }
  .notice p {
    margin: 0;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .grow {
    flex: 1;
    min-width: 0;
  }
  .recents {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-top: 6px;
  }
  .chip {
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 2px 8px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: var(--panel);
    color: var(--text-2);
    font-size: 12px;
    cursor: pointer;
  }
  .chip:hover {
    border-color: var(--border-strong);
    color: var(--text);
  }
</style>
