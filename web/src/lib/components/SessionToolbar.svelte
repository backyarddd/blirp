<script lang="ts">
  import ChevronDown from '@lucide/svelte/icons/chevron-down';
  import GitFork from '@lucide/svelte/icons/git-fork';
  import Square from '@lucide/svelte/icons/square';
  import Play from '@lucide/svelte/icons/play';
  import Brain from '@lucide/svelte/icons/brain';
  import Clock from '@lucide/svelte/icons/clock';
  import Ellipsis from '@lucide/svelte/icons/ellipsis';
  import type { Session } from '../api/types.gen';
  import { app } from '../app.svelte';
  import { sessionActions } from '../actions';
  import { actionEnv, sessionOps } from '../manage';
  import type { MenuItem } from '../menu';
  import { agentLabel, canResume, isLive } from '../status';
  import { formatElapsed } from '../time';
  import { handoffPendingLabel } from '../handoff';
  import Menu from './Menu.svelte';
  import MachineBadge from './MachineBadge.svelte';

  let { session }: { session: Session } = $props();

  const live = $derived(isLive(session.status));

  let now = $state(Date.now());
  $effect(() => {
    if (!live) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const elapsed = $derived(formatElapsed((session.ended_at ?? (live ? now : session.last_activity_at)) - session.started_at));

  // Set while a handoff from this session is being prepared (the daemon may summarize it first).
  const handingOff = $derived(app.handoffFrom.has(session.id));
  const continueItems: MenuItem[] = $derived(
    app.agents.map((a) => ({
      label: a.display_name || agentLabel(a.id),
      hint: a.installed ? (a.id === session.agent ? 'same agent' : '') : 'not installed',
      disabled: !a.installed || handingOff,
      // No folder: the daemon starts it in the source session's folder when that exists here.
      onselect: () => void app.launch({ continue_from: session.id, agent: a.id }),
    })),
  );
  // Everything else the session menus offer (actions.ts); stop and resume stay buttons.
  const more = $derived(sessionActions(session, actionEnv(), sessionOps));
  // Resume and stop of a session on another machine are forwarded to it through the hub.
  const resumable = $derived(canResume(session));
</script>

<div class="toolbar" role="toolbar" aria-label="Session actions">
  <MachineBadge machineId={session.machine_id} />
  {#if app.control}
    <Menu items={continueItems} label="Agent: {agentLabel(session.agent)}. Continue in another agent" heading="Continue in…" triggerClass="btn sm agent">
      <span class="agent-dot" aria-hidden="true"></span>{agentLabel(session.agent)}<ChevronDown size={14} aria-hidden="true" />
    </Menu>
  {:else}
    <span class="btn sm agent static"><span class="agent-dot" aria-hidden="true"></span>{agentLabel(session.agent)}</span>
  {/if}
  <span class="elapsed" title={live ? 'Running for' : 'Duration'}><Clock size={14} aria-hidden="true" /><span class="sr-only">{live ? 'Running for' : 'Duration'}</span>{elapsed}</span>
  {#if app.control}
    <button
      type="button"
      class="btn sm"
      title="Start new session from this session: a fresh session that starts with this session's handoff"
      onclick={() => sessionOps.fork(session)}
      disabled={handingOff}
      aria-busy={handingOff}
    >
      <GitFork size={15} aria-hidden="true" />{handingOff
        ? handoffPendingLabel(session, app.health?.machine.id)
        : 'Start new session from this session'}
    </button>
  {/if}
  <button
    type="button"
    class="icon-btn"
    class:on={app.memoryPanel}
    aria-label="Memory panel"
    aria-pressed={app.memoryPanel}
    title="Memory panel"
    onclick={() => app.setMemoryPanel(!app.memoryPanel)}
  >
    <Brain size={17} />
  </button>
  <Menu items={more} label="More session actions" title="More actions" triggerClass="icon-btn" align="right"><Ellipsis size={17} /></Menu>
  {#if !app.control}
    <!-- View-only device: stop and resume would be refused (403 control_not_allowed). -->
  {:else if live && session.origin === 'blirp'}
    <button type="button" class="btn sm danger" onclick={() => sessionOps.stop(session)}><Square size={12} fill="currentColor" aria-hidden="true" />Stop</button>
  {:else if resumable}
    <button type="button" class="btn sm primary" onclick={() => sessionOps.resume(session)}><Play size={12} fill="currentColor" aria-hidden="true" />Resume</button>
  {/if}
</div>

<style>
  .toolbar {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .toolbar :global(.agent) {
    border-radius: 999px;
    margin-right: 4px;
  }
  .static {
    cursor: default;
  }
  .agent-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
  }
  .elapsed {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 0 8px;
    color: var(--text-2);
    font-variant-numeric: tabular-nums;
    font-size: 12.5px;
  }
  .icon-btn.on {
    color: var(--accent);
    background: var(--accent-soft);
  }
  .btn.danger,
  .btn.primary {
    margin-left: 4px;
  }
</style>
