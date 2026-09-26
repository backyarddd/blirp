<script lang="ts">
  import ChevronDown from '@lucide/svelte/icons/chevron-down';
  import FolderOpen from '@lucide/svelte/icons/folder-open';
  import Code from '@lucide/svelte/icons/code';
  import GitFork from '@lucide/svelte/icons/git-fork';
  import Square from '@lucide/svelte/icons/square';
  import Play from '@lucide/svelte/icons/play';
  import Brain from '@lucide/svelte/icons/brain';
  import Clock from '@lucide/svelte/icons/clock';
  import { api } from '../api/client';
  import type { Session } from '../api/types';
  import { app } from '../app.svelte';
  import { agentLabel, canResume, isLive, sessionTitle } from '../status';
  import { formatElapsed } from '../time';
  import Menu, { type MenuItem } from './Menu.svelte';

  let { session }: { session: Session } = $props();

  let busy = $state(false);
  const live = $derived(isLive(session.status));

  let now = $state(Date.now());
  $effect(() => {
    if (!live) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const elapsed = $derived(formatElapsed((session.ended_at ?? (live ? now : session.last_activity_at)) - session.started_at));

  const continueItems: MenuItem[] = $derived(
    app.agents.map((a) => ({
      label: a.name || agentLabel(a.id),
      hint: a.installed ? (a.id === session.agent ? 'same agent' : '') : 'not installed',
      disabled: !a.installed,
      onselect: () => void app.launch({ continue_from: session.id, agent: a.id, project_id: session.project_id }),
    })),
  );

  async function stop(): Promise<void> {
    if (!confirm(`Stop "${sessionTitle(session)}"? This ends the agent process and everything it started.`)) return;
    busy = true;
    const s = await app.act(() => api.sessions.stop(session.id));
    busy = false;
    if (s) app.upsertSession(s);
  }

  async function resume(): Promise<void> {
    busy = true;
    const s = await app.act(() => api.sessions.resume(session.id));
    busy = false;
    if (s) app.upsertSession(s);
  }
</script>

<div class="toolbar" role="toolbar" aria-label="Session actions">
  <Menu items={continueItems} label="Agent: {agentLabel(session.agent)}. Continue in another agent" heading="Continue in…" triggerClass="btn sm agent">
    <span class="agent-dot" aria-hidden="true"></span>{agentLabel(session.agent)}<ChevronDown size={14} aria-hidden="true" />
  </Menu>
  <span class="elapsed" title={live ? 'Running for' : 'Duration'}><Clock size={14} aria-hidden="true" /><span class="sr-only">{live ? 'Running for' : 'Duration'}</span>{elapsed}</span>
  <button type="button" class="icon-btn" aria-label="Open folder" title="Open folder" onclick={() => app.act(() => api.sessions.open(session.id, 'folder'))}>
    <FolderOpen size={17} />
  </button>
  <button type="button" class="icon-btn" aria-label="Open in editor" title="Open in editor" onclick={() => app.act(() => api.sessions.open(session.id, 'editor'))}>
    <Code size={17} />
  </button>
  <button
    type="button"
    class="icon-btn"
    aria-label="Fork session"
    title="Fork: new session with this session's handoff"
    onclick={() => app.launch({ continue_from: session.id, agent: session.agent, project_id: session.project_id })}
  >
    <GitFork size={17} />
  </button>
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
  {#if live && session.origin === 'blirp'}
    <button type="button" class="btn sm danger" onclick={stop} disabled={busy}><Square size={12} fill="currentColor" aria-hidden="true" />Stop</button>
  {:else if canResume(session)}
    <button type="button" class="btn sm primary" onclick={resume} disabled={busy}><Play size={12} fill="currentColor" aria-hidden="true" />Resume</button>
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
