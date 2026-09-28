<script lang="ts">
  import { untrack } from 'svelte';
  import { api, errorMessage } from '../../lib/api/client';
  import type { ProjectSummary, Session, SessionStatus } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';
  import SessionRow from '../../lib/components/SessionRow.svelte';
  import BulkBar from '../../lib/components/BulkBar.svelte';
  import ListChecks from '@lucide/svelte/icons/list-checks';
  import { markedFirst, sessionArchived, sessionKey } from '../../lib/marks';
  import { Selection } from '../../lib/selection.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const STATUSES: SessionStatus[] = ['working', 'idle', 'waiting', 'completed', 'failed', 'detached'];
  let status: SessionStatus | '' = $state('');
  let q = $state('');
  let appliedQ = $state('');

  let items: Session[] = $state.raw([]);
  let cursor: string | null = $state(null);
  let loading = $state(false);
  let error: string | null = $state(null);
  let token = 0;

  async function load(reset: boolean): Promise<void> {
    const t = ++token;
    loading = true;
    error = null;
    try {
      const page = await api.sessions.list({
        project: pid,
        ...(status ? { status } : {}),
        ...(appliedQ ? { q: appliedQ } : {}),
        ...(!reset && cursor ? { cursor } : {}),
        limit: 50,
      });
      if (t !== token) return;
      // Sort keys (activity, status) change between pages, so a session can come back twice.
      const seen = new Set(reset ? [] : items.map((s) => s.id));
      items = [...(reset ? [] : items), ...page.items.filter((s) => !seen.has(s.id) && seen.add(s.id))];
      cursor = page.next_cursor;
    } catch (e) {
      if (t === token) error = errorMessage(e);
    } finally {
      if (t === token) loading = false;
    }
  }

  $effect(() => {
    void pid;
    void status;
    void appliedQ;
    untrack(() => load(true));
  });

  const selection = new Selection();
  // The live copy when this client has one: a session moved out of the project leaves the list.
  const shown = $derived(
    markedFirst(
      items
        .filter((s) => !app.deletedSessions.has(s.id))
        .map((s) => app.sessionById.get(s.id) ?? s)
        .filter((s) => s.project_id === pid && (app.showArchived || !sessionArchived(s, app.archived, app.projectById))),
      (s) => app.pinned.has(sessionKey(s.id)),
    ),
  );
  const order = $derived(shown.map((s) => s.id));
</script>

<form
  class="row wrap filters"
  onsubmit={(e) => {
    e.preventDefault();
    appliedQ = q.trim();
  }}
>
  <input class="input q" type="search" placeholder="Filter by title or text" aria-label="Filter sessions" bind:value={q} />
  <select class="select st" bind:value={status} aria-label="Status">
    <option value="">Any status</option>
    {#each STATUSES as s (s)}<option value={s}>{s[0]?.toUpperCase()}{s.slice(1)}</option>{/each}
  </select>
  <button class="btn" type="submit">Apply</button>
  <label class="toggle small">
    <input type="checkbox" bind:checked={app.showArchived} />
    <span>Show archived</span>
  </label>
  {#if !selection.active}
    <button type="button" class="btn ghost sm" onclick={() => selection.start()}><ListChecks size={14} aria-hidden="true" />Select</button>
  {/if}
</form>
{#if selection.active}<div class="bulk"><BulkBar {selection} {shown} /></div>{/if}

<div class="card panel-pad">
  <Loadable
    {loading}
    error={items.length === 0 ? error : null}
    empty={shown.length === 0}
    emptyText={status || appliedQ ? 'No sessions match these filters.' : 'No sessions in this project yet.'}
    onretry={() => load(true)}
  >
    <ul class="list">
      {#each shown as s (s.id)}
        <li>
          <SessionRow
            session={s}
            selecting={selection.active}
            selected={selection.ids.has(s.id)}
            onpick={(shift) => selection.toggle(s.id, shift, order)}
          />
        </li>
      {/each}
    </ul>
    {#if error}<p class="err" role="alert">{error}</p>{/if}
    {#if cursor}
      <button type="button" class="btn more" disabled={loading} onclick={() => load(false)}>{loading ? 'Loading…' : 'Load more'}</button>
    {/if}
  </Loadable>
</div>

<style>
  .filters {
    margin-bottom: 12px;
  }
  .toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--text-2);
  }
  .bulk {
    margin-bottom: 12px;
  }
  .q {
    flex: 1;
    min-width: 200px;
    width: auto;
  }
  .st {
    width: auto;
  }
  .more {
    margin-top: 10px;
  }
  .err {
    color: var(--danger);
  }
</style>
