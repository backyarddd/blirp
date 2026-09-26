<script lang="ts">
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import { api, errorMessage } from '../api/client';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { href } from '../router';
  import { childCount, isSubagent, sessionTitle, type SessionExtras } from '../status';
  import { formatRelative } from '../time';
  import StatusChip from './StatusChip.svelte';

  let { parent, onnavigate }: { parent: Session & SessionExtras; onnavigate?: () => void } = $props();

  const count = $derived(childCount(parent, app.sessions));
  let open = $state(false);
  let children: Session[] = $state.raw([]);
  let loading = $state(false);

  async function toggle(): Promise<void> {
    open = !open;
    if (!open) return;
    // Daemons that send `children_count` hide children from lists and filter by `parent`;
    // older ones reject that query but list children with everything else.
    if (typeof parent.children_count !== 'number') {
      children = app.sessions.filter((c) => c.parent_session_id === parent.id && isSubagent(c));
      return;
    }
    loading = true;
    try {
      const page = await api.sessions.list({ parent: parent.id, limit: 100 });
      children = page.items.filter((c) => c.parent_session_id === parent.id);
    } catch (e) {
      app.toast(`Could not load subagents: ${errorMessage(e)}`);
      open = false;
    } finally {
      loading = false;
    }
  }
</script>

{#if count > 0}
  <div class="subs">
    <button type="button" class="toggle" aria-expanded={open} onclick={toggle}>
      <ChevronRight size={13} aria-hidden="true" class={open ? 'rot' : ''} />{count}
      {count === 1 ? 'subagent' : 'subagents'}
    </button>
    {#if open}
      {#if loading && children.length === 0}
        <p class="faint small pad">Loading…</p>
      {:else if children.length === 0}
        <p class="faint small pad">No subagent sessions found.</p>
      {:else}
        <ul class="list-plain">
          {#each children as c (c.id)}
            {@const live = app.sessionById.get(c.id) ?? c}
            <li>
              <a class="child" href={href.sessions(c.id)} onclick={() => onnavigate?.()}>
                <span class="ellipsis">{sessionTitle(live)}</span>
                <span class="faint small">{formatRelative(live.last_activity_at)}</span>
                <StatusChip session={live} />
              </a>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  </div>
{/if}

<style>
  .subs {
    margin: 2px 0 0 10px;
  }
  .toggle {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    border: 0;
    background: none;
    padding: 2px 4px;
    font: inherit;
    font-size: 12px;
    color: var(--text-2);
    cursor: pointer;
    border-radius: 6px;
  }
  .toggle:hover {
    color: var(--text);
  }
  .toggle :global(.rot) {
    transform: rotate(90deg);
  }
  .list-plain {
    list-style: none;
    margin: 2px 0 0;
    padding: 0 0 0 8px;
    border-left: 1px solid var(--border);
    display: grid;
    gap: 2px;
  }
  .child {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 6px;
    border-radius: 6px;
    text-decoration: none;
    font-size: 12.5px;
  }
  .child:hover {
    background: var(--panel-2);
  }
  .child .ellipsis {
    flex: 1;
    min-width: 0;
  }
  .pad {
    margin: 2px 0 0 12px;
  }
</style>
