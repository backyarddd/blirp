<script lang="ts">
  // The git worktrees blirp made for sessions on this machine (`blirp worktrees list`): their changes,
  // Prune for the clean ones of ended sessions, Remove per worktree (forced only after the
  // uncommitted-changes dialog, as from the session's menu).
  import Scissors from '@lucide/svelte/icons/scissors';
  import FolderX from '@lucide/svelte/icons/folder-x';
  import { api } from '../lib/api/client';
  import type { Session, WorktreeInfo } from '../lib/api/types.gen';
  import { app } from '../lib/app.svelte';
  import { dialogs } from '../lib/dialogs.svelte';
  import { removeWorktreeNow } from '../lib/manage';
  import { Resource } from '../lib/resource.svelte';
  import { href } from '../lib/router';
  import { isLive, sessionTitle } from '../lib/status';
  import { formatRelative } from '../lib/time';
  import Loadable from '../lib/components/Loadable.svelte';

  const list = new Resource(() => api.worktrees.list());
  $effect(() => {
    void list.load();
  });

  // The force dialog of a dirty worktree closes after a force removal (or Cancel): read again.
  let forcing = false;
  $effect(() => {
    const open = dialogs.dirtyWorktree !== null;
    if (forcing && !open) void list.reload();
    forcing = open;
  });

  let busy = $state(false);

  async function prune(): Promise<void> {
    busy = true;
    const out = await app.act(() => api.worktrees.prune());
    busy = false;
    if (!out) return;
    const removed = out.filter((w) => w.removed).length;
    app.toast(`Removed ${removed} ${removed === 1 ? 'worktree' : 'worktrees'}; kept ${out.length - removed}.`, 'info');
    void list.reload();
  }

  async function remove(s: Session, w: WorktreeInfo): Promise<void> {
    const ok = await dialogs.confirm({
      title: 'Remove worktree?',
      body: `Remove the git worktree of "${sessionTitle(s)}" at ${w.path}? The folder is deleted; its branch is kept.`,
      confirm: 'Remove worktree',
      danger: true,
    });
    if (!ok) return;
    busy = true;
    await removeWorktreeNow(s, false);
    busy = false;
    void list.reload();
  }

  const changes = (w: WorktreeInfo): string =>
    w.state === 'changed'
      ? `${w.changes} uncommitted ${w.changes === 1 ? 'change' : 'changes'}`
      : w.state === 'unknown'
        ? `state unknown: ${w.error ?? 'git status failed'}`
        : w.state === 'missing'
          ? 'folder missing'
          : 'clean';
</script>

<div class="page">
  <div class="page-inner">
    <nav class="crumbs small" aria-label="Breadcrumb"><a href={href.settings('agents')}>Settings</a> / <span aria-current="page">Worktrees</span></nav>
    <div class="row wrap head">
      <h1 class="page-title">Session worktrees</h1>
      <span class="spacer"></span>
      {#if app.control}
        <button type="button" class="btn" disabled={busy} onclick={prune}><Scissors size={15} aria-hidden="true" />Prune</button>
      {/if}
    </div>
    <p class="muted sub">
      Git worktrees blirp made for sessions on this machine, in <code>~/.blirp/worktrees</code>. Prune removes those of ended sessions
      without uncommitted changes. Removing a worktree deletes its folder; its <code>blirp/…</code> branch and commits are kept.
    </p>
    <Loadable
      loading={list.loading && !list.data}
      error={list.error}
      empty={(list.data ?? []).length === 0}
      emptyText="No blirp worktrees on this machine."
      onretry={() => list.load()}
    >
      <ul class="rows card" aria-label="Worktrees">
        {#each list.data ?? [] as w (w.path)}
          {@const s = w.session}
          <li class="row wrap">
            <div class="main">
              {#if s}
                <a class="ellipsis" href={href.sessions(s.id)}><strong>{sessionTitle(s)}</strong></a>
              {:else}
                <strong>No session</strong>
              {/if}
              <span class="mono small ellipsis" title={w.path}>{w.path}</span>
              <span class="small muted">
                <span class="badge" class:warn={w.state === 'changed' || w.state === 'unknown'}>{changes(w)}</span>
                {#if s}
                  {s.status}{#if s.branch} · <span class="mono">{s.branch}</span>{/if} · started {formatRelative(s.started_at)}
                {:else}
                  no session refers to it; blirp never removes it
                {/if}
              </span>
            </div>
            {#if s && app.control && !isLive(s.status)}
              <button type="button" class="btn sm danger" disabled={busy} onclick={() => remove(s, w)}
                ><FolderX size={14} aria-hidden="true" />Remove…</button
              >
            {/if}
          </li>
        {/each}
      </ul>
    </Loadable>
  </div>
</div>

<style>
  .head {
    margin-bottom: 4px;
  }
  .sub {
    margin: 0 0 16px;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 4px 16px;
  }
  .rows li {
    gap: 12px;
    padding: 12px 0;
    border-bottom: 1px solid var(--border);
  }
  .rows li:last-child {
    border-bottom: 0;
  }
  .main {
    display: grid;
    gap: 4px;
    flex: 1;
    min-width: 0;
  }
  .badge.warn {
    color: var(--warn, var(--danger));
  }
</style>
