<script lang="ts">
  import Send from '@lucide/svelte/icons/send';
  import { api } from '../../lib/api/client';
  import type { ProjectMemory, ProjectSummary, Record as MemoryRecord } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { Resource } from '../../lib/resource.svelte';
  import { href } from '../../lib/router';
  import { agentLabel } from '../../lib/status';
  import BriefEditor from '../../lib/components/BriefEditor.svelte';
  import Loadable from '../../lib/components/Loadable.svelte';
  import RecordItem from '../../lib/components/RecordItem.svelte';
  import SessionRow from '../../lib/components/SessionRow.svelte';

  let { project }: { project: ProjectSummary } = $props();
  const pid = $derived(project.id);

  const memory = new Resource(() => api.projects.memory(pid));
  $effect(() => {
    void app.memoryTick[pid];
    void memory.reload();
  });

  const threads = $derived(
    (memory.data?.records ?? [])
      .filter((r) => r.kind === 'open_thread' && r.status === 'active')
      .sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.updated_at - a.updated_at),
  );

  const installed = $derived(app.agents.filter((a) => a.installed));
  let agent = $state('');
  let prompt = $state('');
  let worktree = $state(false);
  let starting = $state(false);

  $effect(() => {
    if (!agent && installed[0]) agent = installed[0].id;
  });

  function patch(fn: (m: ProjectMemory) => ProjectMemory): void {
    if (memory.data) memory.data = fn(memory.data);
  }

  function onRecord(id: string, r: MemoryRecord | null): void {
    patch((m) => ({
      ...m,
      records: r ? m.records.map((x) => (x.id === id ? r : x)) : m.records.filter((x) => x.id !== id),
    }));
  }

  async function start(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    if (!agent) return;
    starting = true;
    await app.launch({
      project_id: pid,
      agent,
      ...(prompt.trim() ? { prompt: prompt.trim() } : {}),
      ...(project.is_git && worktree ? { worktree: true } : {}),
    });
    starting = false;
  }
</script>

<div class="grid">
  <div class="col">
    <form class="card panel-pad start" onsubmit={start}>
      <label for="ov-prompt" class="h">Start a session</label>
      <textarea
        id="ov-prompt"
        class="textarea"
        rows="3"
        bind:value={prompt}
        placeholder="Describe the task. The agent starts with this project's memory."
        onkeydown={(e) => {
          if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            e.currentTarget.form?.requestSubmit();
          }
        }}
      ></textarea>
      <div class="row wrap">
        <select class="select agent" bind:value={agent} aria-label="Agent">
          {#if app.agents.length === 0}<option value="">{app.agentsLoaded ? 'No agents detected' : 'Detecting agents…'}</option>{/if}
          {#each app.agents as a (a.id)}
            <option value={a.id} disabled={!a.installed}>{a.display_name || agentLabel(a.id)}{a.installed ? '' : ' (not installed)'}</option>
          {/each}
        </select>
        {#if project.is_git}
          <label class="check small"><input type="checkbox" bind:checked={worktree} />New worktree</label>
        {/if}
        <span class="spacer"></span>
        <button type="submit" class="btn primary" disabled={starting || !agent}><Send size={14} aria-hidden="true" />{starting ? 'Starting…' : 'Start'}</button>
      </div>
    </form>

    <section class="card panel-pad">
      <h2 class="h">Brief</h2>
      <Loadable loading={memory.loading} error={memory.error} empty={!memory.data} onretry={() => memory.load()}>
        <BriefEditor projectId={pid} brief={memory.data?.brief ?? null} onsaved={(b) => patch((m) => ({ ...m, brief: b }))} />
      </Loadable>
    </section>
  </div>

  <div class="col">
    <section class="card panel-pad">
      <div class="row">
        <h2 class="h">Open threads</h2>
        <span class="spacer"></span>
        <a class="small link" href={href.project(pid, 'memory')}>Manage</a>
      </div>
      <Loadable loading={memory.loading} error={memory.error} empty={threads.length === 0} emptyText="No open threads.">
        <ul class="list">
          {#each threads as r (r.id)}
            <li><RecordItem record={r} onchange={(x) => onRecord(r.id, x)} /></li>
          {/each}
        </ul>
      </Loadable>
    </section>

    <section class="card panel-pad">
      <div class="row">
        <h2 class="h">Recent sessions</h2>
        <span class="spacer"></span>
        <a class="small link" href={href.project(pid, 'sessions')}>All sessions</a>
      </div>
      <Loadable
        loading={memory.loading}
        error={memory.error}
        empty={(memory.data?.recent_sessions.length ?? 0) === 0}
        emptyText="No sessions in this project yet."
      >
        <ul class="list">
          {#each memory.data?.recent_sessions ?? [] as s (s.id)}
            <li><SessionRow session={app.sessionById.get(s.id) ?? s} /></li>
          {/each}
        </ul>
      </Loadable>
    </section>
  </div>
</div>

<style>
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
    gap: 16px;
    align-items: start;
  }
  .col {
    display: grid;
    gap: 16px;
    min-width: 0;
  }
  .h {
    display: block;
    font-size: 15px;
    font-weight: 700;
    margin: 0 0 10px;
  }
  .start {
    display: grid;
    gap: 10px;
  }
  .start .h {
    margin: 0;
  }
  .agent {
    width: auto;
    min-width: 180px;
  }
  .link {
    color: var(--accent);
    text-decoration: none;
    margin-bottom: 10px;
  }
  @media (max-width: 860px) {
    .grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
