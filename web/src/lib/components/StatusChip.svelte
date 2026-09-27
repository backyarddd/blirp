<script lang="ts">
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { formatRelative } from '../time';
  import { remoteLiveState, sessionStatusInfo, type StatusInfo } from '../status';

  let { session }: { session: Pick<Session, 'status' | 'stopped_by_user' | 'machine_id' | 'last_activity_at'> } = $props();
  // Another machine's live status is only its last report: once that machine is offline (or,
  // with its presence unknown, silent for long) say so instead of claiming it still works.
  const remote = $derived(remoteLiveState(session, app.liveContext()));
  const OFFLINE: StatusInfo = { label: 'Offline', tone: 'detached', pulse: false };
  const NO_UPDATE: StatusInfo = { label: 'No update', tone: 'detached', pulse: false };
  const info = $derived(remote === 'offline' ? OFFLINE : remote === 'stale' ? NO_UPDATE : sessionStatusInfo(session));
  const title = $derived.by(() => {
    if (remote === null) return undefined;
    const last = sessionStatusInfo(session).label;
    const name = app.machineName(session.machine_id);
    if (remote === 'offline') {
      const seen = app.machineById.get(session.machine_id)?.last_seen;
      return `${last} when ${name} went offline${seen ? ` (last seen ${formatRelative(seen)})` : ''}`;
    }
    return `${last} on ${name}, no update since ${formatRelative(session.last_activity_at)}; it may be offline`;
  });
</script>

<span class="chip {info.tone}" {title}>
  <span class="dot" class:pulse={info.pulse} aria-hidden="true"></span>{info.label}
</span>

<style>
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 22px;
    padding: 0 8px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 600;
    white-space: nowrap;
    flex: none;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: currentColor;
  }
  .dot.pulse {
    animation: pulse 1.4s ease-in-out infinite;
  }
  .working {
    color: var(--working);
    background: var(--working-bg);
  }
  .idle {
    color: var(--idle);
    background: var(--idle-bg);
  }
  .waiting {
    color: var(--waiting);
    background: var(--waiting-bg);
  }
  .completed {
    color: var(--completed);
    background: var(--completed-bg);
  }
  .failed {
    color: var(--failed);
    background: var(--failed-bg);
  }
  .detached {
    color: var(--detached);
    background: var(--detached-bg);
    border: 1px dashed var(--border-strong);
  }
</style>
