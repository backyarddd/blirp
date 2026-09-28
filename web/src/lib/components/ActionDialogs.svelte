<script lang="ts">
  import { app } from '../app.svelte';
  import { dialogs } from '../dialogs.svelte';
  import { addProjectFolder, mergeProject, moveSessions, removeWorktreeNow, renameProject, renameSession, type MoveTarget } from '../manage';
  import { agentLabel, isChats, sessionTitle } from '../status';
  import Modal from './Modal.svelte';
  import FolderPicker from './FolderPicker.svelte';

  // ---- Confirm
  const confirming = $derived(dialogs.confirming);

  // ---- Rename (session or project)
  let renameValue = $state('');
  let renameBusy = $state(false);
  const renaming = $derived(dialogs.renaming);
  $effect(() => {
    const r = dialogs.renaming;
    if (r) renameValue = r.kind === 'session' ? (r.session.title ?? '') : r.project.name;
  });

  async function rename(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const r = dialogs.renaming;
    if (!r) return;
    renameBusy = true;
    const ok = r.kind === 'session' ? await renameSession(r.session, renameValue) : await renameProject(r.project, renameValue);
    renameBusy = false;
    if (ok) dialogs.renaming = null;
  }

  // ---- Move sessions: into another project, back to Chats, or into a new project without a folder.
  const NEW = '+new';
  const CHATS = '+chats';
  let moveTo = $state('');
  let moveName = $state('');
  let moveBusy = $state(false);
  const moving = $derived(dialogs.moving);
  const movingOne = $derived(moving?.length === 1 ? moving[0] : undefined);
  const allInChats = $derived(moving?.every((s) => isChats(s.project_id, app.projectById)) ?? false);
  const moveTargets = $derived(
    app.realProjects
      .filter((p) => movingOne === undefined || p.id !== movingOne.project_id)
      .sort((a, b) => a.name.localeCompare(b.name)),
  );
  $effect(() => {
    if (dialogs.moving) {
      moveTo = '';
      moveName = '';
    }
  });

  async function move(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const list = dialogs.moving;
    const name = moveName.trim();
    if (!list || !moveTo || (moveTo === NEW && !name)) return;
    const target: MoveTarget = moveTo === NEW ? { kind: 'new', name } : moveTo === CHATS ? { kind: 'chats' } : { kind: 'project', id: moveTo };
    moveBusy = true;
    await moveSessions(list, target);
    moveBusy = false;
    dialogs.moving = null;
  }

  // ---- Merge project
  let mergeInto = $state('');
  let mergeBusy = $state(false);
  const merging = $derived(dialogs.merging);
  const mergeTargets = $derived(
    app.realProjects.filter((p) => p.id !== merging?.id).sort((a, b) => a.name.localeCompare(b.name)),
  );
  const mergeTarget = $derived(app.projectById.get(mergeInto));
  $effect(() => {
    if (dialogs.merging) mergeInto = '';
  });

  async function merge(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const from = dialogs.merging;
    const into = mergeTarget;
    if (!from || !into) return;
    mergeBusy = true;
    const ok = await mergeProject(from, into);
    mergeBusy = false;
    if (ok) dialogs.merging = null;
  }

  // ---- Add folder (this machine)
  let folder = $state('');
  let folderError: string | null = $state(null);
  let folderBusy = $state(false);
  let browsing = $state(false);
  const addingFolder = $derived(dialogs.addingFolder);
  $effect(() => {
    if (dialogs.addingFolder) {
      folder = '';
      folderError = null;
      browsing = false;
    }
  });

  async function addFolder(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const p = dialogs.addingFolder;
    const path = folder.trim();
    if (!p || !path) return;
    folderBusy = true;
    folderError = await addProjectFolder(p, path);
    folderBusy = false;
    if (folderError === null) dialogs.addingFolder = null;
  }

  // ---- Continue in another agent
  const continuing = $derived(dialogs.continuing);

  function continueWith(agent: string): void {
    const s = dialogs.continuing;
    dialogs.continuing = null;
    // No folder: the daemon starts it in the source session's folder when that exists there.
    if (s) void app.launch({ continue_from: s.id, agent });
  }

  // ---- Dirty worktree
  const dirty = $derived(dialogs.dirtyWorktree);
  let forceBusy = $state(false);

  async function forceRemove(): Promise<void> {
    const d = dialogs.dirtyWorktree;
    if (!d) return;
    forceBusy = true;
    await removeWorktreeNow(d.session, true);
    forceBusy = false;
  }
</script>

<Modal open={confirming !== null} title={confirming?.title ?? ''} onclose={() => dialogs.settleConfirm(false)}>
  {#if confirming}
    <p class="confirm-body">{confirming.body}</p>
    {#if confirming.list && confirming.list.length > 0}
      <ul class="confirm-list small">
        {#each confirming.list.slice(0, 10) as item}<li class="ellipsis">{item}</li>{/each}
        {#if confirming.list.length > 10}<li class="muted">and {confirming.list.length - 10} more</li>{/if}
      </ul>
    {/if}
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => dialogs.settleConfirm(false)}>Cancel</button>
    <button type="button" class="btn {confirming?.danger ? 'danger' : 'primary'}" onclick={() => dialogs.settleConfirm(true)}
      >{confirming?.confirm ?? 'OK'}</button
    >
  {/snippet}
</Modal>

<Modal
  open={renaming !== null}
  title={renaming?.kind === 'project' ? 'Rename project' : 'Rename session'}
  onclose={() => (dialogs.renaming = null)}
>
  {#if renaming}
    <form id="rename-item" onsubmit={rename}>
      <label class="field">
        <span>{renaming.kind === 'project' ? 'Project name' : 'Session title'}</span>
        <!-- svelte-ignore a11y_autofocus -->
        <input
          class="input"
          bind:value={renameValue}
          maxlength={renaming.kind === 'project' ? 200 : 300}
          required={renaming.kind === 'project'}
          placeholder={renaming.kind === 'session' ? sessionTitle({ ...renaming.session, title: null }) : ''}
          autofocus
        />
        {#if renaming.kind === 'session'}<span class="hint">Leave it empty for the default title.</span>{/if}
      </label>
    </form>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.renaming = null)}>Cancel</button>
    <button type="submit" form="rename-item" class="btn primary" disabled={renameBusy}>Rename</button>
  {/snippet}
</Modal>

<Modal open={moving !== null} title={movingOne ? 'Move session' : 'Move sessions'} onclose={() => (dialogs.moving = null)}>
  {#if moving}
    <form id="move-sessions" onsubmit={move}>
      <label class="field">
        <span>{movingOne ? `Move "${sessionTitle(movingOne)}" to` : `Move ${moving.length} sessions to`}</span>
        <select class="select" bind:value={moveTo} required>
          <option value="" disabled>Pick a project</option>
          {#if !allInChats}<option value={CHATS}>Chats (no project)</option>{/if}
          {#each moveTargets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
          <option value={NEW}>New project…</option>
        </select>
      </label>
      {#if moveTo === NEW}
        <label class="field">
          <span>Project name</span>
          <input class="input" bind:value={moveName} required maxlength="200" />
          <span class="hint">A project without a folder; its new sessions start in a blirp workspace.</span>
        </label>
      {/if}
      <p class="small muted">Subagent sessions and the memory records a session produced move along. The folder it ran in is not registered.</p>
    </form>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.moving = null)}>Cancel</button>
    <button type="submit" form="move-sessions" class="btn primary" disabled={moveBusy || !moveTo}>Move</button>
  {/snippet}
</Modal>

<Modal open={merging !== null} title="Merge project" onclose={() => (dialogs.merging = null)}>
  {#if merging}
    <form id="merge-project" onsubmit={merge}>
      <label class="field">
        <span>Merge "{merging.name}" into</span>
        <select class="select" bind:value={mergeInto} required>
          <option value="" disabled>Pick a project</option>
          {#each mergeTargets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
        </select>
      </label>
      <p class="small">
        Everything in "{merging.name}" moves to {mergeTarget ? `"${mergeTarget.name}"` : 'the project you pick'}: its folders, sessions,
        records, wiki pages (a clashing page name gets a number added), resources and pending suggestions. Its brief moves only when
        the target has none; otherwise the target's brief is kept. "{merging.name}" is then deleted.
      </p>
      <p class="small muted">The change syncs to every paired machine and cannot be undone. Files on disk are not touched.</p>
    </form>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.merging = null)}>Cancel</button>
    <button type="submit" form="merge-project" class="btn primary" disabled={!mergeTarget || mergeBusy}>{mergeBusy ? 'Merging…' : 'Merge'}</button>
  {/snippet}
</Modal>

<Modal open={addingFolder !== null} title="Add folder" wide={browsing} onclose={() => (dialogs.addingFolder = null)}>
  {#if addingFolder}
    <form id="add-folder" onsubmit={addFolder}>
      <label class="field">
        <span>Folder on this machine for "{addingFolder.name}"</span>
        <input class="input mono" bind:value={folder} required placeholder="C:\Users\you\Documents\project" spellcheck="false" />
        <span class="hint">An existing folder, as an absolute path. Inside a git repository the repository's top folder is added.</span>
      </label>
      {#if browsing && app.selfId}
        <FolderPicker
          machineId={app.selfId}
          machineName={app.machineName(app.selfId)}
          start={folder}
          onpick={(path) => {
            folder = path;
            browsing = false;
          }}
        />
      {:else}
        <button type="button" class="btn sm" onclick={() => (browsing = true)}>Browse…</button>
      {/if}
      {#if folderError}<p class="err" role="alert">{folderError}</p>{/if}
    </form>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.addingFolder = null)}>Cancel</button>
    <button type="submit" form="add-folder" class="btn primary" disabled={folderBusy || !folder.trim()}>Add folder</button>
  {/snippet}
</Modal>

<Modal open={continuing !== null} title="Continue in…" onclose={() => (dialogs.continuing = null)}>
  {#if continuing}
    <p class="small muted">A new session that starts with the handoff of "{sessionTitle(continuing)}", in the agent you pick.</p>
    <ul class="agents">
      {#each app.agents as a (a.id)}
        <li>
          <button type="button" class="btn agent" disabled={!a.installed} onclick={() => continueWith(a.id)}>
            <span>{a.display_name || agentLabel(a.id)}</span>
            <span class="faint small">{a.installed ? (a.id === continuing.agent ? 'same agent' : '') : 'not installed'}</span>
          </button>
        </li>
      {/each}
    </ul>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.continuing = null)}>Cancel</button>
  {/snippet}
</Modal>

<Modal open={dirty !== null} title="Uncommitted changes" onclose={() => (dialogs.dirtyWorktree = null)}>
  <p>Git reports: {dirty?.message}.</p>
  <p class="muted">
    Force remove deletes the worktree folder with its uncommitted changes and untracked files. This cannot be undone. The
    <code>blirp/…</code> branch and its commits are kept.
  </p>
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (dialogs.dirtyWorktree = null)}>Cancel</button>
    <button type="button" class="btn danger" onclick={forceRemove} disabled={forceBusy}>Force remove</button>
  {/snippet}
</Modal>

<style>
  .confirm-body {
    margin: 0 0 8px;
  }
  .confirm-list {
    margin: 0;
    padding-left: 18px;
    display: grid;
    gap: 2px;
    max-width: 100%;
  }
  .agents {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: grid;
    gap: 6px;
  }
  .agent {
    width: 100%;
    justify-content: space-between;
  }
  .err {
    color: var(--danger);
    margin: 8px 0 0;
  }
</style>
