<script lang="ts">
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { api } from '../../lib/api/client';
  import type { AgentInfo, Settings } from '../../lib/api/types';
  import { app } from '../../lib/app.svelte';
  import { agentLabel } from '../../lib/status';

  let { settings, onsaved }: { settings: Settings; onsaved: (s: Settings) => void } = $props();

  let refreshing = $state(false);
  let busyAgent: string | null = $state(null);

  $effect(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    refreshing = true;
    await app.refreshAgents();
    refreshing = false;
  }

  async function hooks(a: AgentInfo, install: boolean): Promise<void> {
    if (!install && !confirm(`Remove blirp's hooks from ${a.name}'s global config? Other hooks are left alone.`)) return;
    busyAgent = a.id;
    const updated = await app.act(
      () => (install ? api.agents.installHooks(a.id) : api.agents.uninstallHooks(a.id)),
      install ? `Hooks installed for ${a.name}` : `Hooks removed from ${a.name}`,
    );
    busyAgent = null;
    if (updated) app.agents = app.agents.map((x) => (x.id === updated.id ? updated : x));
  }

  async function patch(p: Parameters<typeof api.settings.patch>[0]): Promise<void> {
    const s = await app.act(() => api.settings.patch(p), 'Saved');
    if (s) onsaved(s);
  }
</script>

<section class="card panel-pad">
  <div class="row">
    <h2 class="h">Detected agents</h2>
    <span class="spacer"></span>
    <button type="button" class="btn sm" onclick={refresh} disabled={refreshing}><RefreshCw size={14} aria-hidden="true" />{refreshing ? 'Detecting…' : 'Re-detect'}</button>
  </div>
  <p class="hint">
    Global hooks let blirp remember sessions you start outside blirp. Installing adds clearly marked entries to the agent's own
    config; uninstalling removes exactly those entries. Sessions started from blirp never need them.
  </p>
  {#if app.agentsError}
    <p class="err" role="alert">{app.agentsError}</p>
  {:else if app.agents.length === 0}
    <p class="muted">{refreshing ? 'Detecting agents…' : 'No agents detected on PATH.'}</p>
  {:else}
    <ul class="list">
      {#each app.agents as a (a.id)}
        <li class="agent">
          <div class="info">
            <div class="row wrap">
              <strong>{a.name || agentLabel(a.id)}</strong>
              {#if a.installed}
                <span class="badge ok">installed{a.version ? ` · ${a.version}` : ''}</span>
              {:else}
                <span class="badge">not found</span>
              {/if}
              {#if a.custom}<span class="badge">custom</span>{/if}
            </div>
            {#if a.path}<span class="mono small faint ellipsis" title={a.path}>{a.path}</span>{/if}
            <span class="small muted">
              {#if !a.integration.supported}
                Global hooks not supported; memory is injected when launched from blirp.
              {:else if a.integration.installed}
                Global hooks installed.
              {:else}
                Global hooks not installed.
              {/if}
              {#if a.integration.detail}<span class="faint"> {a.integration.detail}</span>{/if}
            </span>
          </div>
          {#if a.integration.supported && a.installed}
            {#if a.integration.installed}
              <button type="button" class="btn sm" disabled={busyAgent === a.id} onclick={() => hooks(a, false)}>Uninstall hooks</button>
            {:else}
              <button type="button" class="btn sm primary" disabled={busyAgent === a.id} onclick={() => hooks(a, true)}>Install hooks</button>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<section class="card panel-pad">
  <h2 class="h">Defaults</h2>
  <label class="field top">
    <span>Default agent for new sessions</span>
    <select class="select narrow" value={settings.agents.default} onchange={(e) => patch({ agents: { default: e.currentTarget.value } })}>
      {#each app.agents.filter((a) => a.installed || a.id === settings.agents.default) as a (a.id)}
        <option value={a.id}>{a.name || agentLabel(a.id)}</option>
      {/each}
    </select>
  </label>
  <label class="check">
    <input
      type="checkbox"
      checked={settings.sessions.worktree_default}
      onchange={(e) => patch({ sessions: { worktree_default: e.currentTarget.checked } })}
    />
    <span>Start sessions in git projects in a new worktree by default</span>
  </label>
</section>

<style>
  .h {
    font-size: 15px;
    margin: 0;
  }
  .top {
    margin-top: 12px;
  }
  .agent {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 0;
  }
  .info {
    display: grid;
    gap: 3px;
    flex: 1;
    min-width: 0;
  }
  .badge.ok {
    background: var(--completed-bg);
    color: var(--completed);
  }
  .narrow {
    max-width: 320px;
  }
  .err {
    color: var(--danger);
  }
</style>
