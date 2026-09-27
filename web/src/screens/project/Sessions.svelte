<script lang="ts">
  import { untrack } from 'svelte';
  import { api, errorMessage } from '../../lib/api/client';
  import type { ProjectSummary, Session, SessionStatus } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';
  import SessionRow from '../../lib/components/SessionRow.svelte';

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
</form>

<div class="card panel-pad">
  <Loadable
    {loading}
    error={items.length === 0 ? error : null}
    empty={items.length === 0}
    emptyText={status || appliedQ ? 'No sessions match these filters.' : 'No sessions in this project yet.'}
    onretry={() => load(true)}
  >
    <ul class="list">
      {#each items.filter((s) => !app.deletedSessions.has(s.id)) as s (s.id)}
        <li><SessionRow session={app.sessionById.get(s.id) ?? s} /></li>
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
