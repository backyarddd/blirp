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
  import { ApiError, api, errorMessage } from '../api/client';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { remoteRefusal } from '../capabilities';
  import { agentLabel, canResume, isLive, sessionTitle } from '../status';
  import { formatElapsed } from '../time';
  import Menu, { type MenuItem } from './Menu.svelte';
  import Modal from './Modal.svelte';

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
    await app.act(() => api.sessions.stop(session.id));
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

  // Ended sessions only: the daemon refuses live ones (409 `session_live`). Another machine's
  // session is deleted by that machine: it must be online and accept changes from here.
  async function remove(): Promise<void> {
    const msg = `Delete "${sessionTitle(session)}"? Its transcript and subagent sessions are removed on every synced machine. Memory records it produced stay.`;
    if (!confirm(msg)) return;
    // The `session_deleted` event may unmount this toolbar before the reply arrives.
    const { id, machine_id } = session;
    const isRemote = remote;
    busy = true;
    try {
      await api.sessions.delete(id);
      app.toast('Session deleted', 'info');
      app.removeSession(id);
    } catch (e) {
      const why = isRemote && e instanceof ApiError ? remoteRefusal(e.code, await ownerName(machine_id)) : null;
      if (why === null) app.noteForbidden(e);
      app.toast(`Could not delete the session: ${why ?? errorMessage(e)}`);
    } finally {
      busy = false;
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

  async function resume(): Promise<void> {
    busy = true;
    const s = await app.act(() => api.sessions.resume(session.id));
    busy = false;
    if (s) app.upsertSession(s);
  }
</script>

<div class="toolbar" role="toolbar" aria-label="Session actions">
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
      class="icon-btn"
      aria-label="Fork session"
      title="Fork: new session with this session's handoff"
      onclick={() => app.launch({ continue_from: session.id, agent: session.agent })}
    >
      <GitFork size={17} />
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
