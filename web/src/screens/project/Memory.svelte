<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import History from '@lucide/svelte/icons/history';
  import ListChecks from '@lucide/svelte/icons/list-checks';
  import { api } from '../../lib/api/client';
  import type {
    Brief,
    ProjectSummary,
    Record as MemoryRecord,
    RecordKind,
    RecordStatus,
    Suggestion,
  } from '../../lib/api/types.gen';
  import { proposalMarkdown } from '../../lib/memory';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { href } from '../../lib/router';
  import { formatDateTime, formatRelative } from '../../lib/time';
  import BriefEditor from '../../lib/components/BriefEditor.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';
  import Markdown from '../../lib/components/Markdown.svelte';
  import RecordItem, { KIND_LABEL } from '../../lib/components/RecordItem.svelte';
  import RecordBulkBar from '../../lib/components/RecordBulkBar.svelte';
  import { Selection } from '../../lib/selection.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const memory = new Resource(() => api.projects.memory(pid));
  const records = new Resource(() => api.projects.records(pid));
  const suggestions = new Resource(() => api.projects.suggestions(pid));
  const history = new Resource(() => api.projects.briefHistory(pid));

  $effect(() => {
    void app.memoryTick[pid];
    void memory.reload();
    void records.reload();
    void suggestions.reload();
  });

  // Follows the loaded memory; local saves/reverts override it until the next load.
  let brief: Brief | null = $derived(memory.data?.brief ?? null);

  let showHistory = $state(false);
  $effect(() => {
    if (showHistory) void history.load();
  });

  // Records
  const KINDS: RecordKind[] = ['decision', 'plan', 'note', 'open_thread', 'gotcha'];
  let kind: RecordKind | 'all' = $state('all');
  let statusFilter: RecordStatus | 'all' = $state('active');
  const visible = $derived(
    (records.data ?? [])
      .filter((r) => (kind === 'all' || r.kind === kind) && (statusFilter === 'all' || r.status === statusFilter))
      .sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.updated_at - a.updated_at),
  );
  const counts = $derived.by(() => {
    const c: Record<string, number> = { all: 0 };
    for (const r of records.data ?? []) {
      if (statusFilter !== 'all' && r.status !== statusFilter) continue;
      c.all = (c.all ?? 0) + 1;
      c[r.kind] = (c[r.kind] ?? 0) + 1;
    }
    return c;
  });

  const selection = new Selection();
  const order = $derived(visible.map((r) => r.id));

  let creating = $state(false);
  let newKind: RecordKind = $state('note');
  let newTitle = $state('');
  let newBody = $state('');

  function onRecord(id: string, r: MemoryRecord | null): void {
    const list = records.data ?? [];
    records.data = r ? list.map((x) => (x.id === id ? r : x)) : list.filter((x) => x.id !== id);
  }

  async function create(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const title = newTitle.trim();
    if (!title) return;
    const r = await app.act(() => api.projects.createRecord(pid, { kind: newKind, title, body: newBody.trim() }));
    if (r) {
      records.data = [r, ...(records.data ?? [])];
      newTitle = '';
      newBody = '';
      creating = false;
    }
  }

  // By id: numbers shift when older versions from another machine arrive. Entries replicated by
  // older blirp versions have no id and fall back to the number.
  const isCurrent = (v: Brief): boolean => (v.id && brief?.id ? v.id === brief.id : v.version === brief?.version);

  async function revert(v: Brief): Promise<void> {
    if (!confirm(`Revert the brief to version ${v.version}? The current text stays in history.`)) return;
    const target = v.id ? { id: v.id } : { version: v.version };
    const b = await app.act(() => api.projects.revertBrief(pid, target), `Brief reverted to v${v.version}`);
    if (b) {
      brief = b;
      void history.reload();
    }
  }

  async function decide(s: Suggestion, decision: 'accept' | 'reject' | 'dismiss'): Promise<void> {
    const out = await app.act(() => api.suggestions.decide(s.id, decision));
    if (!out) return;
    suggestions.data = (suggestions.data ?? []).filter((x) => x.id !== s.id);
    if (decision === 'accept') {
      void memory.reload();
      void records.reload();
    }
  }

  const pending = $derived((suggestions.data ?? []).filter((s) => s.status === 'pending'));
</script>

<div class="stack">
  <section class="card panel-pad">
    <div class="row">
      <h2 class="h">Brief</h2>
      <span class="spacer"></span>
      <button type="button" class="btn sm ghost" aria-expanded={showHistory} onclick={() => (showHistory = !showHistory)}>
        <History size={14} aria-hidden="true" />History
      </button>
    </div>
    <Loadable loading={memory.loading} error={memory.error} empty={!memory.data} onretry={() => memory.load()}>
      <BriefEditor projectId={pid} {brief} onsaved={(b) => (brief = b)} />
    </Loadable>
    {#if showHistory}
      <div class="history">
        <h3 class="section-title">Versions</h3>
        <Loadable loading={history.loading} error={history.error} empty={(history.data?.length ?? 0) === 0} emptyText="No earlier versions." onretry={() => history.load()}>
          <ul class="list">
            {#each history.data ?? [] as v (v.id || v.version)}
              <li class="ver">
                <details>
                  <summary>
                    <strong>v{v.version}</strong>
                    <span class="muted small">{v.updated_by} · {formatDateTime(v.updated_at)}</span>
                  </summary>
                  <div class="ver-body"><Markdown source={v.body_md || '_Empty_'} /></div>
                </details>
                {#if !isCurrent(v) && app.control}
                  <button type="button" class="btn sm" onclick={() => revert(v)}>Revert</button>
                {:else if isCurrent(v)}
                  <span class="badge accent">current</span>
                {/if}
              </li>
            {/each}
          </ul>
        </Loadable>
      </div>
    {/if}
  </section>

  <section class="card panel-pad">
    <div class="row wrap">
      <h2 class="h">Suggestions</h2>
      {#if pending.length}<span class="badge accent">{pending.length} pending</span>{/if}
    </div>
    <p class="hint top">The distiller proposes changes here instead of overwriting anything you edited yourself.</p>
    <Loadable
      loading={suggestions.loading}
      error={suggestions.error}
      empty={pending.length === 0}
      emptyText="No pending suggestions."
      onretry={() => suggestions.load()}
    >
      <ul class="list">
        {#each pending as s (s.id)}
          <li class="sugg">
            <div class="row wrap">
              <span class="badge">{s.target === 'record' ? 'Record' : s.target === 'brief' ? 'Brief' : 'Wiki'}</span>
              <span class="muted small">{formatRelative(s.created_at)}</span>
              {#if s.source_session_id}<a class="small" href={href.sessions(s.source_session_id)}>from session</a>{/if}
            </div>
            {#if s.rationale}<p class="small rationale">{s.rationale}</p>{/if}
            <div class="proposal"><Markdown source={proposalMarkdown(s)} class="small" /></div>
            {#if app.control}
              <div class="row">
                <span class="spacer"></span>
                <button type="button" class="btn sm ghost" onclick={() => decide(s, 'dismiss')}>Dismiss</button>
                <button type="button" class="btn sm" onclick={() => decide(s, 'reject')}>Reject</button>
                <button type="button" class="btn sm primary" onclick={() => decide(s, 'accept')}>Accept</button>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    </Loadable>
  </section>

  <section class="card panel-pad">
    <div class="row wrap">
      <h2 class="h">Records</h2>
      <span class="spacer"></span>
      <select class="select st" bind:value={statusFilter} aria-label="Record status">
        <option value="active">Active</option>
        <option value="resolved">Resolved</option>
        <option value="archived">Archived</option>
        <option value="all">All</option>
      </select>
      {#if app.control && !selection.active}
        <button type="button" class="btn ghost sm" onclick={() => selection.start()}><ListChecks size={14} aria-hidden="true" />Select</button>
      {/if}
      {#if app.control}
        <button type="button" class="btn sm primary" onclick={() => (creating = !creating)} aria-expanded={creating}>
          <Plus size={14} aria-hidden="true" />New record
        </button>
      {/if}
    </div>
    <div class="pills kinds" role="tablist" aria-label="Record kind">
      <button type="button" role="tab" class="pill" aria-selected={kind === 'all'} onclick={() => (kind = 'all')}>All {counts.all ?? 0}</button>
      {#each KINDS as k (k)}
        <button type="button" role="tab" class="pill" aria-selected={kind === k} onclick={() => (kind = k)}>{KIND_LABEL[k]} {counts[k] ?? 0}</button>
      {/each}
    </div>

    {#if selection.active && app.control}<div class="bulk"><RecordBulkBar {selection} shown={visible} /></div>{/if}

    {#if creating && app.control}
      <form class="create" onsubmit={create}>
        <div class="row wrap">
          <select class="select kind-sel" bind:value={newKind} aria-label="Kind">
            {#each KINDS as k (k)}<option value={k}>{KIND_LABEL[k]}</option>{/each}
          </select>
          <input class="input grow" placeholder="Title" aria-label="Title" bind:value={newTitle} required />
        </div>
        <textarea class="textarea" rows="3" placeholder="Details (markdown)" aria-label="Details" bind:value={newBody}></textarea>
        <div class="row">
          <span class="spacer"></span>
          <button type="button" class="btn sm" onclick={() => (creating = false)}>Cancel</button>
          <button type="submit" class="btn sm primary" disabled={!newTitle.trim()}>Create</button>
        </div>
      </form>
    {/if}

    <Loadable
      loading={records.loading}
      error={records.error}
      empty={visible.length === 0}
      emptyText="No records here. Decisions, plans, notes, open threads and gotchas appear as sessions are distilled."
      onretry={() => records.load()}
    >
      <ul class="list">
        {#each visible as r (r.id)}
          <li>
            <RecordItem
              record={r}
              showKind={kind === 'all'}
              onchange={(x) => onRecord(r.id, x)}
              selecting={selection.active}
              selected={selection.ids.has(r.id)}
              onpick={(shift) => selection.toggle(r.id, shift, order)}
            />
          </li>
        {/each}
      </ul>
    </Loadable>
  </section>
</div>

<style>
  .stack {
    display: grid;
    gap: 16px;
  }
  .h {
    font-size: 15px;
    margin: 0;
  }
  .top {
    margin: 4px 0 8px;
  }
  .history {
    margin-top: 14px;
    border-top: 1px solid var(--border);
    padding-top: 12px;
  }
  .ver {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 8px 0;
  }
  .ver details {
    flex: 1;
    min-width: 0;
  }
  .ver summary {
    cursor: pointer;
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  .ver-body {
    margin-top: 8px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    max-height: 320px;
    overflow: auto;
  }
  .sugg {
    display: grid;
    gap: 8px;
    padding: 12px 0;
  }
  .rationale {
    margin: 0;
    color: var(--text-2);
  }
  .proposal {
    padding: 10px 12px;
    border-radius: var(--radius-sm);
    background: var(--panel-2);
    border: 1px solid var(--border);
    max-height: 260px;
    overflow: auto;
  }
  .bulk {
    margin-top: 12px;
  }
  .kinds {
    margin: 12px 0 4px;
    flex-wrap: wrap;
    border-radius: 14px;
  }
  .st,
  .kind-sel {
    width: auto;
  }
  .grow {
    flex: 1;
    min-width: 180px;
    width: auto;
  }
  .create {
    display: grid;
    gap: 8px;
    margin: 12px 0;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--panel-2);
  }
</style>
