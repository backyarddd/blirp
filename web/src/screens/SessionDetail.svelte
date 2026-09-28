<script lang="ts">
  import { untrack } from 'svelte';
  import User from '@lucide/svelte/icons/user';
  import Bot from '@lucide/svelte/icons/bot';
  import Wrench from '@lucide/svelte/icons/wrench';
  import FileText from '@lucide/svelte/icons/file-text';
  import FilePen from '@lucide/svelte/icons/file-pen';
  import Info from '@lucide/svelte/icons/info';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import Pencil from '@lucide/svelte/icons/pencil';
  import { renameSession } from '../lib/manage';
  import { api, errorMessage } from '../lib/api/client';
  import type { Event as SessionEvent, EventKind, Session } from '../lib/api/types.gen';
  import { parseSummary } from '../lib/memory';
  import { app } from '../lib/app.svelte';
  import { agentLabel, sessionTitle } from '../lib/status';
  import { formatDateTime, formatElapsed, formatRelative, formatTime } from '../lib/time';
  import Markdown from '../lib/components/Markdown.svelte';
  import StatusChip from '../lib/components/StatusChip.svelte';
  import Loadable from '../lib/components/Loadable.svelte';

  let { session }: { session: Session } = $props();

  const PAGE = 200;
  // Derived ids so status pushes (new session objects, same id) don't refetch everything.
  const sid = $derived(session.id);
  let events: SessionEvent[] = $state.raw([]);
  let eventsLoading = $state(false);
  let eventsError: string | null = $state(null);
  /** `after` cursor for the next page; null once the last page is loaded. */
  // Seqs start at 0 and pages hold the events after `after`.
  let nextAfter: number | null = $state(-1);

  $effect(() => {
    const id = sid;
    untrack(() => {
      events = [];
      nextAfter = -1;
      void loadMore(id);
    });
  });

  async function loadMore(id: string): Promise<void> {
    eventsLoading = true;
    eventsError = null;
    const after = nextAfter ?? -1;
    try {
      const page = await api.sessions.events(id, after, PAGE);
      if (id !== sid) return;
      events = [...events, ...page.items];
      nextAfter = page.next_after;
    } catch (e) {
      if (id === sid) eventsError = errorMessage(e);
    } finally {
      if (id === sid) eventsLoading = false;
    }
  }

  // Session rows (list, detail and pushed updates) carry the summary; no extra fetch needed.
  const summary = $derived(parseSummary(session.summary));

  // Inline rename of the title: Enter saves (an empty title goes back to the default), Esc cancels.
  let editing = $state(false);
  let draft = $state('');
  let saving = $state(false);
  let titleInput: HTMLInputElement | undefined = $state();
  $effect(() => {
    void sid;
    editing = false;
  });
  $effect(() => {
    if (editing) titleInput?.select();
  });

  async function saveTitle(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    saving = true;
    const ok = await renameSession(session, draft);
    saving = false;
    if (ok) editing = false;
  }

  let busy = $state(false);
  async function distill(): Promise<void> {
    busy = true;
    await app.distill(session);
    busy = false;
  }

  const ICONS: Record<EventKind, typeof User> = {
    user: User,
    assistant: Bot,
    tool_call: Wrench,
    tool_result: FileText,
    system: Info,
    file_edit: FilePen,
    summary: Sparkles,
  };
  const LABELS: Record<EventKind, string> = {
    user: 'You',
    assistant: 'Agent',
    tool_call: 'Tool call',
    tool_result: 'Tool result',
    system: 'System',
    file_edit: 'File edit',
    summary: 'Summary',
  };
</script>

<div class="detail">
  <header class="head">
    <div class="titles">
      {#if editing}
        <form class="rename" onsubmit={saveTitle}>
          <input
            bind:this={titleInput}
            class="input"
            bind:value={draft}
            aria-label="Session title"
            maxlength="300"
            placeholder={sessionTitle({ ...session, title: null })}
            onkeydown={(e) => {
              if (e.key === 'Escape') {
                e.preventDefault();
                editing = false;
              }
            }}
          />
          <button type="submit" class="btn sm primary" disabled={saving}>Save</button>
          <button type="button" class="btn sm" onclick={() => (editing = false)}>Cancel</button>
        </form>
      {:else}
        <div class="title-row">
          <h1 class="ellipsis">{sessionTitle(session)}</h1>
          {#if app.control}
            <button
              type="button"
              class="icon-btn sm"
              aria-label="Rename session"
              title="Rename"
              onclick={() => {
                draft = session.title ?? '';
                editing = true;
              }}><Pencil size={14} /></button
            >
          {/if}
        </div>
      {/if}
      <div class="row wrap small muted">
        <StatusChip {session} />
        <span>{agentLabel(session.agent)}</span>
        {#if session.origin === 'external'}<span class="badge">started outside blirp</span>{/if}
        <span>{formatDateTime(session.started_at)}</span>
        {#if session.ended_at}<span>· {formatElapsed(session.ended_at - session.started_at)}</span>{/if}
        {#if session.tokens_in + session.tokens_out > 0}
          <span>· {(session.tokens_in + session.tokens_out).toLocaleString()} tokens</span>
        {/if}
        {#if session.cost_usd > 0}<span>· ${session.cost_usd.toFixed(2)}</span>{/if}
        {#if session.exit_code !== null && session.exit_code !== 0}<span>· exit code {session.exit_code}</span>{/if}
      </div>
    </div>
    {#if app.control}
      <div class="row wrap actions">
        <button type="button" class="btn sm" onclick={distill} disabled={busy}>Distill now</button>
      </div>
    {/if}
  </header>

  <div class="body">
    {#if summary?.error}
      <p class="err small" role="status">
        Last distill failed {formatRelative(summary.error.at)}: {summary.error.message}
      </p>
    {/if}
    {#if summary?.summary}
      <section class="card summary">
        <h2 class="section-title">Summary</h2>
        <p>{summary.summary}</p>
        <div class="cols">
          {#if summary.decisions.length}
            <div>
              <h3>Decisions</h3>
              <ul>{#each summary.decisions as d}<li><strong>{d.title}</strong> {d.body}</li>{/each}</ul>
            </div>
          {/if}
          {#if summary.open_threads.length}
            <div>
              <h3>Open threads</h3>
              <ul>{#each summary.open_threads as d}<li><strong>{d.title}</strong> {d.body}</li>{/each}</ul>
            </div>
          {/if}
          {#if summary.gotchas.length}
            <div>
              <h3>Gotchas</h3>
              <ul>{#each summary.gotchas as d}<li><strong>{d.title}</strong> {d.body}</li>{/each}</ul>
            </div>
          {/if}
          {#if summary.files.length}
            <div>
              <h3>Files</h3>
              <ul class="files">{#each summary.files as f}<li class="mono">{f}</li>{/each}</ul>
            </div>
          {/if}
        </div>
      </section>
    {:else}
      <p class="muted small">No summary yet. blirp distills sessions after they go idle or end.</p>
    {/if}

    <h2 class="section-title transcript-title">Transcript</h2>
    <Loadable
      loading={eventsLoading}
      error={events.length === 0 ? eventsError : null}
      empty={events.length === 0}
      emptyText="No transcript events were recorded for this session."
      onretry={() => loadMore(session.id)}
    >
      <ol class="events">
        {#each events as ev (ev.seq)}
          {@const Icon = ICONS[ev.kind]}
          <li class="ev {ev.kind}">
            <div class="ev-head">
              <Icon size={14} aria-hidden="true" />
              <span class="kind">{LABELS[ev.kind]}</span>
              <time class="faint" datetime={new Date(ev.ts).toISOString()}>{formatTime(ev.ts)}</time>
            </div>
            {#if ev.kind === 'assistant' || ev.kind === 'summary'}
              <Markdown source={ev.text} />
            {:else if ev.kind === 'tool_result' && ev.text.length > 400}
              <details>
                <summary>{ev.text.slice(0, 120)}…</summary>
                <pre>{ev.text}</pre>
              </details>
            {:else if ev.kind === 'user'}
              <p class="text">{ev.text}</p>
            {:else}
              <pre>{ev.text}</pre>
            {/if}
          </li>
        {/each}
      </ol>
      {#if eventsError}<p class="err" role="alert">{eventsError}</p>{/if}
      {#if nextAfter !== null}
        <button type="button" class="btn more" onclick={() => loadMore(session.id)} disabled={eventsLoading}>
          {eventsLoading ? 'Loading…' : 'Load more'}
        </button>
      {/if}
    </Loadable>
  </div>
</div>

<style>
  .detail {
    height: 100%;
    overflow: auto;
  }
  .head {
    position: sticky;
    top: 0;
    z-index: 2;
    display: flex;
    align-items: flex-start;
    gap: 12px;
    flex-wrap: wrap;
    justify-content: space-between;
    padding: 16px 20px 12px;
    background: var(--panel);
    border-bottom: 1px solid var(--border);
  }
  .titles {
    min-width: 0;
    flex: 1;
  }
  h1 {
    font-size: 18px;
    margin: 0;
    min-width: 0;
  }
  .title-row,
  .rename {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 0 0 6px;
    min-width: 0;
  }
  .rename .input {
    flex: 1;
    min-width: 0;
  }
  .body {
    padding: 16px 20px 24px;
    max-width: 920px;
  }
  .summary {
    padding: 14px 16px;
    margin-bottom: 20px;
    background: var(--panel-2);
  }
  .summary p {
    margin: 0 0 8px;
  }
  .cols {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: 8px 20px;
  }
  .cols h3 {
    font-size: 13px;
    margin: 8px 0 4px;
  }
  .cols ul {
    margin: 0;
    padding-left: 18px;
    font-size: 13px;
  }
  .files {
    list-style: none;
    padding-left: 0 !important;
  }
  .transcript-title {
    margin-top: 8px;
  }
  .events {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 10px;
  }
  .ev {
    padding: 8px 12px;
    border-radius: 10px;
    border: 1px solid transparent;
    min-width: 0;
  }
  .ev-head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-2);
    margin-bottom: 4px;
  }
  .ev-head time {
    margin-left: auto;
    font-weight: 400;
  }
  .ev.user {
    background: var(--accent-soft);
    border-color: color-mix(in srgb, var(--accent) 25%, transparent);
  }
  .ev.user .kind {
    color: var(--accent);
  }
  .ev.assistant {
    border-color: var(--border);
  }
  .ev.tool_call,
  .ev.tool_result,
  .ev.file_edit {
    background: var(--panel-2);
    padding: 6px 12px;
  }
  .ev.system {
    opacity: 0.75;
  }
  .ev.summary {
    border-color: var(--completed);
    background: var(--completed-bg);
  }
  .text {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  pre {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-size: 12.5px;
    color: var(--text-2);
  }
  details summary {
    cursor: pointer;
    font-family: var(--mono);
    font-size: 12.5px;
    color: var(--text-2);
    overflow-wrap: anywhere;
  }
  .more {
    margin-top: 12px;
  }
  .err {
    color: var(--danger);
  }
</style>
