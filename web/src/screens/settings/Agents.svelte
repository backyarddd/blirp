<script lang="ts">
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { api } from '../../lib/api/client';
  import type { Config, SettingsView } from '../../lib/api/types.gen';
  import type { AgentView } from '../../lib/api/types.pending';
  import { app } from '../../lib/app.svelte';
  import { agentLabel } from '../../lib/status';

  let { settings, onsaved }: { settings: SettingsView; onsaved: (s: SettingsView) => void } = $props();

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

  async function hooks(a: AgentView, install: boolean): Promise<void> {
    if (!install && !confirm(`Remove blirp's hooks from ${a.display_name}'s global config? Other hooks are left alone.`)) return;
    busyAgent = a.id;
    const updated = await app.act(
      () => (install ? api.agents.installHooks(a.id) : api.agents.uninstallHooks(a.id)),
      install ? `Hooks installed for ${a.display_name}` : `Hooks removed from ${a.display_name}`,
    );
    busyAgent = null;
    if (updated) app.agents = app.agents.map((x) => (x.id === updated.id ? updated : x));
  }

  // PATCH replaces the whole config, so every change starts from the loaded one.
  async function saveConfig(change: (c: Config) => Config): Promise<void> {
    const s = await app.act(() => api.settings.patch({ config: change(settings.config) }), 'Saved');
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
              <strong>{a.display_name || agentLabel(a.id)}</strong>
              {#if a.installed}
                <span class="badge ok">installed{a.version ? ` · ${a.version}` : ''}</span>
              {:else}
                <span class="badge">not found</span>
              {/if}
              {#if !a.builtin}<span class="badge">custom</span>{/if}
            </div>
            {#if a.path}<span class="mono small faint ellipsis" title={a.path}>{a.path}</span>{/if}
            <span class="small muted">
              {#if !a.builtin}
                Custom command: memory is injected when launched from blirp.
              {:else if !a.integration}
                Global integration status is not available in this version of blirp.
              {:else}
                Global hooks {a.integration.global_hooks ? 'installed' : 'not installed'} · MCP {a.integration.mcp
                  ? 'registered'
                  : 'not registered'} · memory injection {a.integration.inject ? 'on' : 'off'}
              {/if}
            </span>
          </div>
          {#if a.builtin && a.installed && a.integration}
            {#if a.integration.global_hooks}
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
    <select
      class="select narrow"
      value={settings.config.agents.default}
      onchange={(e) => {
        const value = e.currentTarget.value;
        void saveConfig((c) => ({ ...c, agents: { ...c.agents, default: value } }));
      }}
    >
      {#each app.agents.filter((a) => a.installed || a.id === settings.config.agents.default) as a (a.id)}
        <option value={a.id}>{a.display_name || agentLabel(a.id)}</option>
      {/each}
    </select>
  </label>
  <label class="check">
    <input
      type="checkbox"
      checked={settings.config.sessions.worktree_default}
      onchange={(e) => {
        const on = e.currentTarget.checked;
        void saveConfig((c) => ({ ...c, sessions: { ...c.sessions, worktree_default: on } }));
      }}
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
