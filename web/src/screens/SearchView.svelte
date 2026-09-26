<script lang="ts">
  import SearchIcon from '@lucide/svelte/icons/search';
  import MessageSquare from '@lucide/svelte/icons/message-square';
  import BookMarked from '@lucide/svelte/icons/book-marked';
  import { api } from '../lib/api/client';
  import type { SearchHit, SearchHitKind, SearchResults } from '../lib/api/types.gen';
  import { app } from '../lib/app.svelte';
  import { Resource } from '../lib/resource.svelte';
  import { navigate } from '../lib/router.svelte';
  import { href } from '../lib/router';
  import { splitSnippet } from '../lib/markdown';
  import { agentLabel } from '../lib/status';
  import { formatRelative } from '../lib/time';
  import Loadable from '../lib/components/Loadable.svelte';

  let { q, project, kind }: { q: string; project: string | null; kind: string | null } = $props();

  const validKind = (k: string | null): SearchHitKind | undefined => (k === 'event' || k === 'record' ? k : undefined);

  // Form state follows the URL; submitting writes the URL, which drives the query.
  let text = $derived(q);
  let projectSel = $derived(project ?? '');
  let kindSel = $derived(validKind(kind) ?? '');

  const results = new Resource(() =>
    q.trim()
      ? api.search({ q: q.trim(), ...(project ? { project } : {}), ...(validKind(kind) ? { kind: validKind(kind) } : {}) })
      : Promise.resolve<SearchResults>({ hits: [] }),
  );
  $effect(() => {
    void results.load();
  });

  function submit(e: SubmitEvent): void {
    e.preventDefault();
    navigate(href.search(text.trim(), projectSel || null, kindSel || null));
  }

  const hits = $derived(results.data?.hits ?? []);
  const hitKey = (h: SearchHit): string => `${h.kind}:${h.session_id ?? ''}:${h.seq ?? ''}:${h.record_id ?? ''}`;
  const hitTitle = (h: SearchHit): string =>
    h.title?.trim() || (h.kind === 'event' ? `${h.agent ? agentLabel(h.agent) : 'Session'} transcript` : 'Untitled record');

  function hitHref(h: SearchHit): string {
    if (h.kind === 'event' && h.session_id) return href.sessions(h.session_id);
    return href.project(h.project_id, 'memory');
  }
</script>

<div class="page">
  <div class="page-inner narrow">
    <h1 class="page-title">Search</h1>
    <p class="muted sub">Full-text search across every transcript and memory record, on every synced machine.</p>
    <form class="bar card" role="search" onsubmit={submit}>
      <SearchIcon size={18} aria-hidden="true" />
      <!-- svelte-ignore a11y_autofocus -->
      <input class="q" type="search" bind:value={text} placeholder="Search sessions and memory" aria-label="Search query" autofocus />
      <select class="select f" bind:value={projectSel} aria-label="Project">
        <option value="">All projects</option>
        {#each app.projects as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
      </select>
      <select class="select f" bind:value={kindSel} aria-label="Result type">
        <option value="">Everything</option>
        <option value="event">Transcripts</option>
        <option value="record">Memory records</option>
      </select>
      <button type="submit" class="btn primary">Search</button>
    </form>

    {#if !q.trim()}
      <p class="muted hint-empty">Try a function name, an error message, or a decision you remember making.</p>
    {:else}
      <Loadable
        loading={results.loading}
        error={results.error}
        empty={hits.length === 0}
        emptyText="No matches for “{q}”."
        onretry={() => results.load()}
      >
        <p class="small muted count" role="status">{hits.length} {hits.length === 1 ? 'result' : 'results'}</p>
        <ul class="hits">
          {#each hits as h (hitKey(h))}
            <li>
              <a class="hit card" href={hitHref(h)}>
                <span class="icon" aria-hidden="true">
                  {#if h.kind === 'event'}<MessageSquare size={16} />{:else}<BookMarked size={16} />{/if}
                </span>
                <span class="body">
                  <span class="row wrap top">
                    <strong class="ellipsis">{hitTitle(h)}</strong>
                    <span class="badge">{h.kind === 'event' ? 'Transcript' : 'Memory record'}</span>
                  </span>
                  <span class="snippet"
                    >{#each splitSnippet(h.snippet) as part, i (i)}{#if part.match}<mark>{part.text}</mark>{:else}{part.text}{/if}{/each}</span
                  >
                  <span class="small faint">
                    {app.projectById.get(h.project_id)?.name ?? 'Unknown project'}{h.agent ? ` · ${agentLabel(h.agent)}` : ''} · {formatRelative(h.ts)}
                  </span>
                </span>
              </a>
            </li>
          {/each}
        </ul>
      </Loadable>
    {/if}
  </div>
</div>

<style>
  .narrow {
    max-width: 860px;
  }
  .sub {
    margin: 0 0 16px;
  }
  .bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    padding: 8px 8px 8px 14px;
    color: var(--text-2);
  }
  .q {
    flex: 1;
    min-width: 160px;
    border: 0;
    background: transparent;
    font-size: 15px;
    padding: 6px 0;
    outline: none;
    color: var(--text);
  }
  .f {
    width: auto;
  }
  .hint-empty,
  .count {
    margin: 16px 2px 8px;
  }
  .hits {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 8px;
  }
  .hit {
    display: flex;
    gap: 12px;
    padding: 12px 14px;
    text-decoration: none;
  }
  .hit:hover {
    border-color: var(--border-strong);
  }
  .icon {
    color: var(--text-3);
    padding-top: 2px;
  }
  .body {
    display: grid;
    gap: 4px;
    min-width: 0;
    flex: 1;
  }
  .snippet {
    color: var(--text-2);
    font-size: 13px;
    overflow-wrap: anywhere;
  }
</style>
