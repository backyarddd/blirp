<script lang="ts">
  import X from '@lucide/svelte/icons/x';
  import Plus from '@lucide/svelte/icons/plus';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import { api } from '../api/client';
  import type { ProjectMemory, Record as MemoryRecord, Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { parseSummary } from '../memory';
  import { Resource } from '../resource.svelte';
  import { href } from '../router';
  import { formatRelative } from '../time';
  import BriefEditor from './BriefEditor.svelte';
  import Loadable from './Loadable.svelte';
  import Markdown from './Markdown.svelte';
  import RecordItem from './RecordItem.svelte';

  let { session }: { session: Session } = $props();

  // Derived ids so status pushes (new session objects, same ids) don't refetch.
  const sid = $derived(session.id);
  const pid = $derived(session.project_id);
  const injection = new Resource(() => api.inject(sid));
  const memory = new Resource(() => api.projects.memory(pid));
  /** Chats have no project memory: nothing is injected, and a chat's summary stays with it. */
  const chats = $derived(app.projectById.get(pid)?.chats === true);

  $effect(() => {
    void injection.load();
  });
  // Switching project clears the panel; a memory update for the same project refreshes in place.
  let loadedFor = '';
  $effect(() => {
    const id = pid;
    void app.memoryTick[id];
    if (id !== loadedFor) {
      loadedFor = id;
      void memory.load();
    } else {
      void memory.reload();
      // Sessions without a launch file (external ones) get a fresh render of the memory.
      void injection.reload();
    }
  });

  const threads = $derived(
    (memory.data?.records ?? [])
      .filter((r) => r.kind === 'open_thread' && r.status === 'active')
      .sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.updated_at - a.updated_at),
  );
  // Pinned open threads are already listed above.
  const pinned = $derived(
    (memory.data?.records ?? []).filter((r) => r.pinned && r.status === 'active' && r.kind !== 'open_thread'),
  );

  const summary = $derived(parseSummary(session.summary));
  let distilling = $state(false);
  async function distill(): Promise<void> {
    distilling = true;
    await app.distill(session);
    distilling = false;
  }

  let adding = $state(false);
  let newTitle = $state('');
  let newBody = $state('');
  let raw = $state(false);

  function patchMemory(fn: (m: ProjectMemory) => ProjectMemory): void {
    if (memory.data) memory.data = fn(memory.data);
  }

  function onRecord(id: string, r: MemoryRecord | null): void {
    patchMemory((m) => ({
      ...m,
      records: r ? m.records.map((x) => (x.id === id ? r : x)) : m.records.filter((x) => x.id !== id),
    }));
  }

  async function addThread(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const title = newTitle.trim();
    if (!title) return;
    const r = await app.act(() =>
      api.projects.createRecord(pid, { kind: 'open_thread', title, body: newBody.trim() }),
    );
    if (r) {
      patchMemory((m) => ({ ...m, records: [r, ...m.records] }));
      newTitle = '';
      newBody = '';
      adding = false;
    }
  }
</script>

<div class="panel">
  <header class="head">
    <h2>Memory</h2>
    {#if !chats}<a class="small" href={href.project(pid, 'memory')}>All memory</a>{/if}
    <button type="button" class="icon-btn sm" aria-label="Close memory panel" onclick={() => app.setMemoryPanel(false)}><X size={16} /></button>
  </header>

  <div class="scroll">
    <section>
      <div class="row">
        <h3 class="section-title">Injected at start</h3>
        <span class="spacer"></span>
        {#if injection.data?.markdown}
          <button type="button" class="btn sm ghost" aria-pressed={raw} onclick={() => (raw = !raw)}>{raw ? 'Rendered' : 'Raw'}</button>
        {/if}
      </div>
      <Loadable
        loading={injection.loading}
        error={injection.error}
        empty={!injection.data?.markdown}
        emptyText="Nothing was injected into this session."
        onretry={() => injection.load()}
      >
        <details class="inj">
          <summary>{(injection.data?.markdown.length ?? 0).toLocaleString()} characters</summary>
          {#if raw}
            <pre>{injection.data?.markdown}</pre>
          {:else}
            <Markdown source={injection.data?.markdown ?? ''} class="small" />
          {/if}
        </details>
      </Loadable>
    </section>

    <section aria-label="Distill">
      <div class="row">
        <h3 class="section-title">Session summary</h3>
        <span class="spacer"></span>
        {#if app.control}
          <button type="button" class="btn sm" onclick={distill} disabled={distilling}>
            <Sparkles size={13} aria-hidden="true" />{distilling ? 'Queuing…' : 'Distill now'}
          </button>
        {/if}
      </div>
      {#if summary?.summary}
        <p class="small">{summary.summary}</p>
        {#if summary.distilled_at}
          <p class="faint small">Distilled {formatRelative(summary.distilled_at)}{summary.backend ? ` by ${summary.backend}` : ''}</p>
        {/if}
      {:else}
        <p class="muted small">Not distilled yet. blirp distills sessions after they go idle or end.</p>
      {/if}
      {#if summary?.error}
        <p class="distill-err small" role="status" data-testid="distill-error">
          Last distill failed {formatRelative(summary.error.at)}: {summary.error.message}
        </p>
      {/if}
    </section>

    {#if chats}
      <p class="muted small" data-testid="chats-memory">
        This session is a chat: it belongs to no project, so it gets no project memory and adds none. Move it to a project
        (the folder icon above) when it turns out to be part of one.
      </p>
    {:else}
    <Loadable loading={memory.loading} error={memory.error} empty={!memory.data} onretry={() => memory.load()}>
      <section>
        <h3 class="section-title">Brief</h3>
        <BriefEditor
          compact
          projectId={pid}
          brief={memory.data?.brief ?? null}
          onsaved={(b) => patchMemory((m) => ({ ...m, brief: b }))}
        />
      </section>

      <section>
        <div class="row">
          <h3 class="section-title">Open threads</h3>
          <span class="spacer"></span>
          {#if app.control}
            <button type="button" class="icon-btn sm" aria-label="Add open thread" title="Add open thread" onclick={() => (adding = !adding)}>
              <Plus size={15} />
            </button>
          {/if}
        </div>
        {#if adding && app.control}
          <form class="add" onsubmit={addThread}>
            <input class="input" placeholder="What is still open?" aria-label="Thread title" bind:value={newTitle} required />
            <textarea class="textarea" rows="3" placeholder="Details (optional)" aria-label="Thread details" bind:value={newBody}></textarea>
            <div class="row">
              <span class="spacer"></span>
              <button type="button" class="btn sm" onclick={() => (adding = false)}>Cancel</button>
              <button type="submit" class="btn sm primary" disabled={!newTitle.trim()}>Add</button>
            </div>
          </form>
        {/if}
        {#if threads.length === 0}
          <p class="muted small">No open threads.</p>
        {:else}
          <ul class="list">
            {#each threads as r (r.id)}
              <li><RecordItem record={r} onchange={(x) => onRecord(r.id, x)} /></li>
            {/each}
          </ul>
        {/if}
      </section>

      {#if pinned.length > 0}
        <section>
          <h3 class="section-title">Pinned</h3>
          <ul class="list">
            {#each pinned as r (r.id)}
              <li><RecordItem record={r} onchange={(x) => onRecord(r.id, x)} /></li>
            {/each}
          </ul>
        </section>
      {/if}
    </Loadable>
    {/if}
  </div>
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 12px 8px 16px;
    border-bottom: 1px solid var(--border);
  }
  .head h2 {
    font-size: 15px;
    margin: 0;
    flex: 1;
  }
  .head a {
    color: var(--accent);
    text-decoration: none;
  }
  .scroll {
    flex: 1;
    overflow: auto;
    padding: 4px 16px 16px;
  }
  section {
    padding: 12px 0;
    border-bottom: 1px solid var(--border);
  }
  section:last-child {
    border-bottom: 0;
  }
  .section-title {
    margin: 0 0 6px;
  }
  .inj summary {
    cursor: pointer;
    color: var(--text-2);
    font-size: 12.5px;
    margin-bottom: 6px;
  }
  .inj pre {
    white-space: pre-wrap;
    font-size: 12px;
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 8px;
    max-height: 360px;
    overflow: auto;
  }
  .distill-err {
    color: var(--failed);
    margin: 6px 0 0;
    overflow-wrap: anywhere;
  }
  .add {
    display: grid;
    gap: 6px;
    margin-bottom: 8px;
  }
</style>
