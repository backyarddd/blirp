<script lang="ts">
  import GitFork from '@lucide/svelte/icons/git-fork';
  import X from '@lucide/svelte/icons/x';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { compactionDismissedAt, compactionHintVisible, contextSignal, dismissCompactionHint, handoffPendingLabel } from '../handoff';
  import { formatRelative } from '../time';

  // Mounted per session (keyed by the parent). The agent compacted its context (its window was
  // full and older turns are now a summary), or its context is nearly full. Suggest the same
  // action as the toolbar's "Start new session from this session", once per such signal.
  let { session }: { session: Session } = $props();

  /** Set by a dismissal here; else what this browser remembers. */
  let dismissedHere: number | null = $state(null);
  const dismissedAt = $derived(dismissedHere ?? compactionDismissedAt(session.id));
  const visible = $derived(app.control && compactionHintVisible(session, dismissedAt));
  const handingOff = $derived(app.handoffFrom.has(session.id));
  const signal = $derived(contextSignal(session));

  function dismiss(): void {
    if (signal === null) return;
    dismissCompactionHint(session.id, signal.at);
    dismissedHere = signal.at;
  }
</script>

{#if visible && signal !== null}
  {@const when = formatRelative(signal.at, Math.max(app.clock, signal.at))}
  <div class="hint" role="region" aria-label="Context nearly full" data-testid="compaction-hint">
    <span class="text">
      {#if signal.kind === 'near_full'}
        The agent's context was over 90% full {when}: it will soon compact older turns into a summary.
      {:else}
        The agent compacted its context {when}: older turns are now a summary.
      {/if}
      A new session that starts from this one's handoff has room again.
    </span>
    <button
      type="button"
      class="btn sm primary"
      onclick={() => app.launch({ continue_from: session.id, agent: session.agent })}
      disabled={handingOff}
      aria-busy={handingOff}
    >
      <GitFork size={14} aria-hidden="true" />{handingOff
        ? handoffPendingLabel(session, app.health?.machine.id)
        : 'Start new session from this session'}
    </button>
    <button type="button" class="icon-btn" aria-label="Dismiss" title="Dismiss until the context fills up again" onclick={dismiss}>
      <X size={15} />
    </button>
  </div>
{/if}

<style>
  .hint {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
    padding: 6px 8px 6px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel-2);
    font-size: 13px;
    min-width: 0;
  }
  .text {
    flex: 1 1 320px;
    min-width: 0;
    color: var(--text-2);
  }
</style>
