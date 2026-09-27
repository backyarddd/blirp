<script lang="ts">
  import ChevronDown from '@lucide/svelte/icons/chevron-down';
  import FolderOpen from '@lucide/svelte/icons/folder-open';
  import Code from '@lucide/svelte/icons/code';
  import GitFork from '@lucide/svelte/icons/git-fork';
  import Square from '@lucide/svelte/icons/square';
  import Play from '@lucide/svelte/icons/play';
  import Brain from '@lucide/svelte/icons/brain';
  import Clock from '@lucide/svelte/icons/clock';
  import Trash from '@lucide/svelte/icons/trash-2';
  import FolderX from '@lucide/svelte/icons/folder-x';
  import FolderInput from '@lucide/svelte/icons/folder-input';
  import { ApiError, api, errorMessage } from '../api/client';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { remoteRefusal } from '../capabilities';
  import { agentLabel, canResume, isLive, sessionTitle } from '../status';
  import { formatElapsed } from '../time';
  import Menu, { type MenuItem } from './Menu.svelte';
  import Modal from './Modal.svelte';
  import MachineBadge from './MachineBadge.svelte';

  let { session }: { session: Session } = $props();

  let busy = $state(false);
  const live = $derived(isLive(session.status));

  let now = $state(Date.now());
  $effect(() => {
    if (!live) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const elapsed = $derived(formatElapsed((session.ended_at ?? (live ? now : session.last_activity_at)) - session.started_at));

  const continueItems: MenuItem[] = $derived(
    app.agents.map((a) => ({
      label: a.display_name || agentLabel(a.id),
      hint: a.installed ? (a.id === session.agent ? 'same agent' : '') : 'not installed',
      disabled: !a.installed,
      // No folder: the daemon starts it in the source session's folder when that exists here.
      onselect: () => void app.launch({ continue_from: session.id, agent: a.id }),
    })),
  );

  // The daemon answers 202 and reports the final status on the event stream.
  async function stop(): Promise<void> {
    if (!confirm(`Stop "${sessionTitle(session)}"? This ends the agent process and everything it started.`)) return;
    busy = true;
    await onSession('stop the session', () => api.sessions.stop(session.id));
    busy = false;
  }

  // Resume and stop of a session on another machine are forwarded to it through the hub.
  const resumable = $derived(canResume(session));
  // Opening a folder shows a window on this machine's desktop: local clients, local sessions.
  const canOpen = $derived(app.admin && session.machine_id === app.health?.machine.id);

  const remote = $derived(session.machine_id !== app.health?.machine.id);

  /** Name of the machine that owns this session, for error messages. */
  async function ownerName(machineId: string): Promise<string> {
    try {
      const m = (await api.machines.list()).find((x) => x.id === machineId);
      if (m) return m.name;
    } catch {
      // Only for the message; fall through to a generic name.
    }
    return 'The machine that runs this session';
  }

  /**
   * Run `action` on this session. Stop, resume and delete of another machine's session are
   * forwarded to it: its refusal (offline, no control from here) is explained, not taken as a
   * change of this client's rights. Returns undefined when it failed (a toast says why).
   */
  async function onSession<T>(what: string, action: () => Promise<T>): Promise<T | undefined> {
    const { machine_id } = session;
    const isRemote = remote;
    try {
      return await action();
    } catch (e) {
      const why = isRemote && e instanceof ApiError ? remoteRefusal(e.code, await ownerName(machine_id)) : null;
      if (why === null) app.noteForbidden(e);
      app.toast(`Could not ${what}: ${why ?? errorMessage(e)}`);
      return undefined;
    }
  }

  // Ended sessions only: the daemon refuses live ones (409 `session_live`). Another machine's
  // session is deleted by that machine: it must be online and accept changes from here.
  async function remove(): Promise<void> {
    const msg = `Delete "${sessionTitle(session)}"? Its transcript and subagent sessions are removed on every synced machine. Memory records it produced stay.`;
    if (!confirm(msg)) return;
    // The `session_deleted` event may unmount this toolbar before the reply arrives.
    const { id } = session;
    busy = true;
    const done = await onSession('delete the session', async () => {
      await api.sessions.delete(id);
      return true;
    });
    busy = false;
    if (done) {
      app.toast('Session deleted', 'info');
      app.removeSession(id);
    }
  }

  // The worktree lives on this machine under BLIRP_HOME/worktrees; the daemon refuses others.
  const canRemoveWorktree = $derived(session.worktree !== null && session.machine_id === app.health?.machine.id);
  /** Set when the worktree has uncommitted changes: the daemon's message, for the force prompt. */
  let dirty: string | null = $state(null);

  async function removeWorktree(force: boolean): Promise<void> {
    if (!force && !confirm(`Remove the git worktree of "${sessionTitle(session)}"? The folder is deleted; its branch is kept.`)) return;
    busy = true;
    try {
      app.upsertSession(await api.sessions.removeWorktree(session.id, force));
      dirty = null;
      app.toast('Worktree removed', 'info');
    } catch (e) {
      if (!force && e instanceof ApiError && e.code === 'worktree_dirty') {
        dirty = e.message;
      } else {
        dirty = null;
        app.noteForbidden(e);
        app.toast(`Could not remove the worktree: ${errorMessage(e)}`);
      }
    } finally {
      busy = false;
    }
  }

  // Move: into another project, back to Chats, or into a new project without a folder.
  const NEW = '+new';
  const CHATS = '+chats';
  let moving = $state(false);
  let moveTo = $state('');
  let newName = $state('');
  const inChats = $derived(app.projectById.get(session.project_id)?.chats === true);
  const moveTargets = $derived(
    app.realProjects.filter((p) => p.id !== session.project_id).sort((a, b) => a.name.localeCompare(b.name)),
  );

  function openMove(): void {
    moveTo = '';
    newName = '';
    moving = true;
  }

  async function move(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const name = newName.trim();
    if (!moveTo || (moveTo === NEW && !name)) return;
    busy = true;
    const moved = await app.act(async () => {
      let target: string | null = null;
      if (moveTo === NEW) {
        const p = await api.projects.create({ name });
        app.upsertProject(p);
        target = p.id;
      } else if (moveTo !== CHATS) {
        target = moveTo;
      }
      return api.sessions.move(session.id, target);
    });
    busy = false;
    if (!moved) return;
    moving = false;
    app.upsertSession(moved);
    const where = moveTo === CHATS ? undefined : app.projectById.get(moved.project_id);
    app.toast(`Moved to ${where?.name ?? 'Chats'}`, 'info');
  }

  async function resume(): Promise<void> {
    busy = true;
    const s = await onSession('resume the session', () => api.sessions.resume(session.id));
    busy = false;
    if (s) app.upsertSession(s);
  }
</script>

<div class="toolbar" role="toolbar" aria-label="Session actions">
  <MachineBadge machineId={session.machine_id} />
  {#if app.control}
    <Menu items={continueItems} label="Agent: {agentLabel(session.agent)}. Continue in another agent" heading="Continue in…" triggerClass="btn sm agent">
      <span class="agent-dot" aria-hidden="true"></span>{agentLabel(session.agent)}<ChevronDown size={14} aria-hidden="true" />
    </Menu>
  {:else}
    <span class="btn sm agent static"><span class="agent-dot" aria-hidden="true"></span>{agentLabel(session.agent)}</span>
  {/if}
  <span class="elapsed" title={live ? 'Running for' : 'Duration'}><Clock size={14} aria-hidden="true" /><span class="sr-only">{live ? 'Running for' : 'Duration'}</span>{elapsed}</span>
  {#if canOpen}
    <button type="button" class="icon-btn" aria-label="Open folder" title="Open folder" onclick={() => app.act(() => api.sessions.open(session.id, 'folder'))}>
      <FolderOpen size={17} />
    </button>
    <button type="button" class="icon-btn" aria-label="Open in editor" title="Open in editor" onclick={() => app.act(() => api.sessions.open(session.id, 'editor'))}>
      <Code size={17} />
    </button>
  {/if}
  {#if app.control}
    <button
      type="button"
      class="btn sm"
      title="Start new session from this session: a fresh session that starts with this session's handoff"
      onclick={() => app.launch({ continue_from: session.id, agent: session.agent })}
    >
      <GitFork size={15} aria-hidden="true" />Start new session from this session
    </button>
  {/if}
  <button
    type="button"
    class="icon-btn"
    class:on={app.memoryPanel}
    aria-label="Memory panel"
    aria-pressed={app.memoryPanel}
    title="Memory panel"
    onclick={() => app.setMemoryPanel(!app.memoryPanel)}
  >
    <Brain size={17} />
  </button>
  {#if app.control}
    <button type="button" class="icon-btn" aria-label="Move to project" title={inChats ? 'Move to a project' : 'Move to another project or Chats'} onclick={openMove} disabled={busy}>
      <FolderInput size={17} />
    </button>
  {/if}
  {#if app.control && !live}
    {#if canRemoveWorktree}
      <button type="button" class="icon-btn" aria-label="Remove worktree" title="Remove worktree" onclick={() => removeWorktree(false)} disabled={busy}>
        <FolderX size={17} />
      </button>
    {/if}
    <button type="button" class="icon-btn" aria-label="Delete session" title="Delete session" onclick={remove} disabled={busy}>
      <Trash size={17} />
    </button>
  {/if}
  {#if !app.control}
    <!-- View-only device: stop and resume would be refused (403 control_not_allowed). -->
  {:else if live && session.origin === 'blirp'}
    <button type="button" class="btn sm danger" onclick={stop} disabled={busy}><Square size={12} fill="currentColor" aria-hidden="true" />Stop</button>
  {:else if resumable}
    <button type="button" class="btn sm primary" onclick={resume} disabled={busy}><Play size={12} fill="currentColor" aria-hidden="true" />Resume</button>
  {/if}
</div>

<Modal open={moving} title="Move session" onclose={() => (moving = false)}>
  <form id="move-session" onsubmit={move}>
    <label class="field">
      <span>Move "{sessionTitle(session)}" to</span>
      <select class="select" bind:value={moveTo} required>
        <option value="" disabled>Pick a project</option>
        {#if !inChats}<option value={CHATS}>Chats (no project)</option>{/if}
        {#each moveTargets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
        <option value={NEW}>New project…</option>
      </select>
    </label>
    {#if moveTo === NEW}
      <label class="field">
        <span>Project name</span>
        <input class="input" bind:value={newName} required maxlength="200" />
        <span class="hint">A project without a folder; its new sessions start in a blirp workspace.</span>
      </label>
    {/if}
    <p class="small muted">Its subagent sessions and the memory records it produced move along. The folder it ran in is not registered.</p>
  </form>
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (moving = false)}>Cancel</button>
    <button type="submit" form="move-session" class="btn primary" disabled={busy || !moveTo}>Move</button>
  {/snippet}
</Modal>

<Modal open={dirty !== null} title="Uncommitted changes" onclose={() => (dirty = null)}>
  <p>Git reports: {dirty}.</p>
  <p class="muted">
    Force remove deletes the worktree folder with its uncommitted changes and untracked files. This cannot be undone. The
    <code>blirp/…</code> branch and its commits are kept.
  </p>
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dirty = null)}>Cancel</button>
    <button type="button" class="btn danger" onclick={() => removeWorktree(true)} disabled={busy}>Force remove</button>
  {/snippet}
</Modal>

<style>
  .toolbar {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .toolbar :global(.agent) {
    border-radius: 999px;
    margin-right: 4px;
  }
  .static {
    cursor: default;
  }
  .agent-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
  }
  .elapsed {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 0 8px;
    color: var(--text-2);
    font-variant-numeric: tabular-nums;
    font-size: 12.5px;
  }
  .icon-btn.on {
    color: var(--accent);
    background: var(--accent-soft);
  }
  .btn.danger,
  .btn.primary {
    margin-left: 4px;
  }
</style>
