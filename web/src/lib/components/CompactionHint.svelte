<script lang="ts">
  import GitFork from '@lucide/svelte/icons/git-fork';
  import X from '@lucide/svelte/icons/x';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { compactionDismissedAt, compactionHintVisible, dismissCompactionHint, handoffPendingLabel } from '../handoff';
  import { formatRelative } from '../time';

  // Mounted per session (keyed by the parent). The agent compacted its context: its window was
  // full and older turns are now a summary. Suggest the same action as the toolbar's "Start new
  // session from this session", once per compaction.
  let { session }: { session: Session } = $props();

  /** Set by a dismissal here; else what this browser remembers. */
  let dismissedHere: number | null = $state(null);
  const dismissedAt = $derived(dismissedHere ?? compactionDismissedAt(session.id));
  const visible = $derived(app.control && compactionHintVisible(session, dismissedAt));
  const handingOff = $derived(app.handoffFrom.has(session.id));

  function dismiss(): void {
    if (session.compacted_at === null) return;
    dismissCompactionHint(session.id, session.compacted_at);
    dismissedHere = session.compacted_at;
  }
</script>

{#if visible && session.compacted_at !== null}
  <div class="hint" role="region" aria-label="Context compacted" data-testid="compaction-hint">
    <span class="text">
      The agent compacted its context {formatRelative(session.compacted_at, Math.max(app.clock, session.compacted_at))}: older turns are now a summary. A new
      session that starts from this one's handoff has room again.
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
    <button type="button" class="icon-btn" aria-label="Dismiss" title="Dismiss until the next compaction" onclick={dismiss}>
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
