<script lang="ts">
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { api } from '../../lib/api/client';
  import type { AgentInfo, AgentToken, Config, InjectMode, IntegrationState, SettingsView } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { agentLabel } from '../../lib/status';
  import Modal from '../../lib/components/Modal.svelte';

  let { settings, onsaved }: { settings: SettingsView; onsaved: (s: SettingsView) => void } = $props();

  let refreshing = $state(false);
  let busyAgent: string | null = $state(null);
  let confirming: AgentInfo | null = $state.raw(null);
  /** claude login token being pasted; never shown back (the daemon never returns it). */
  let tokenInput = $state('');
  let savingToken = $state(false);

  $effect(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    refreshing = true;
    await app.refreshAgents();
    refreshing = false;
  }

  // User config files each agent's global integration edits (ARCHITECTURE §9).
  const FILES: Record<string, string[]> = {
    claude: ['~/.claude/settings.json (hooks)', '~/.claude.json (MCP server)'],
    codex: ['~/.codex/hooks.json (hooks)', '~/.codex/config.toml (MCP server)'],
    gemini: ['~/.gemini/settings.json (hooks and MCP server)'],
    cursor: ['~/.cursor/hooks.json (hooks)', '~/.cursor/mcp.json (MCP server)'],
    opencode: ['~/.config/opencode/opencode.json (MCP server)'],
  };

  const STATE: Record<IntegrationState, string> = {
    installed: 'installed',
    not_installed: 'not installed',
    unsupported: 'not supported',
  };
  const INJECT: Record<InjectMode, string> = {
    hook: 'session-start hook',
    instructions: 'system instructions',
    flag: 'command-line flag',
    none: 'none (BLIRP_MEMORY_FILE only)',
  };

  const supported = (a: AgentInfo): boolean =>
    a.integration.global_hooks !== 'unsupported' || a.integration.mcp !== 'unsupported';
  const anyIs = (a: AgentInfo, st: IntegrationState): boolean =>
    a.integration.global_hooks === st || a.integration.mcp === st;
  const nameOf = (a: AgentInfo): string => a.display_name || agentLabel(a.id);

  async function run(a: AgentInfo, install: boolean): Promise<void> {
    confirming = null;
    busyAgent = a.id;
    const updated = await app.act(
      () => (install ? api.agents.installHooks(a.id) : api.agents.uninstallHooks(a.id)),
      install ? `Global integration installed for ${nameOf(a)}` : `Global integration removed from ${nameOf(a)}`,
    );
    busyAgent = null;
    if (updated) replaceAgent(updated);
  }

  const replaceAgent = (updated: AgentInfo): void => {
    app.agents = app.agents.map((x) => (x.id === updated.id ? updated : x));
  };

  function loginText(a: AgentInfo): string {
    if (!a.auth) return 'unknown';
    return a.auth.logged_in ? `logged in${a.auth.method ? ` (${a.auth.method})` : ''}` : 'not logged in';
  }

  function tokenText(t: AgentToken): string {
    if (t.env) return 'CLAUDE_CODE_OAUTH_TOKEN from the daemon environment';
    return t.stored ? 'stored' : 'none';
  }

  async function saveToken(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const token = tokenInput.trim();
    if (token === '') return;
    savingToken = true;
    const updated = await app.act(() => api.agents.setToken('claude', token), 'Claude login token saved');
    savingToken = false;
    if (updated) {
      tokenInput = '';
      replaceAgent(updated);
    }
  }

  async function clearToken(): Promise<void> {
    if (!confirm('Remove the stored Claude login token? New claude sessions on this machine then use its normal login again.')) return;
    savingToken = true;
    const updated = await app.act(() => api.agents.clearToken('claude'), 'Claude login token removed');
    savingToken = false;
    if (updated) replaceAgent(updated);
  }

  function uninstall(a: AgentInfo): void {
    if (confirm(`Remove blirp's hooks and MCP server from ${nameOf(a)}'s user config? Other entries are left alone.`)) {
      void run(a, false);
    }
  }

  // Sent with its base, so only this change is applied (§11).
  async function saveConfig(change: (c: Config) => Config): Promise<void> {
    const s = await app.saveSettings({ config: change(settings.config), base: settings.config }, 'Saved');
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
    Sessions started from blirp always get memory. The optional global integration covers sessions you start outside blirp: it
    adds blirp's hooks and MCP server to the agent's own user config. The entries are clearly marked, other entries are kept, the
    original file is backed up once, and Uninstall removes exactly what was added.
  </p>
  {#if !app.admin}
    <p class="small muted">Global integration can only be changed from this machine's own desktop app or CLI.</p>
  {/if}
  {#if app.agentsError}
    <p class="err" role="alert">{app.agentsError}</p>
  {:else if app.agents.length === 0}
    <p class="muted">{refreshing ? 'Detecting agents…' : 'No agents detected on PATH.'}</p>
  {:else}
    <ul class="list">
      {#each app.agents as a (a.id)}
        <li class="agent" data-agent={a.id}>
          <div class="info">
            <div class="row wrap">
              <strong>{nameOf(a)}</strong>
              {#if a.installed}
                <span class="badge ok">installed{a.version ? ` · ${a.version}` : ''}</span>
              {:else}
                <span class="badge">not found</span>
              {/if}
              {#if !a.builtin}<span class="badge">custom</span>{/if}
            </div>
            {#if a.path}<span class="mono small faint ellipsis" title={a.path}>{a.path}</span>{/if}
            <dl class="integ small">
              <dt>Global hooks</dt>
              <dd class={a.integration.global_hooks} data-testid="hooks-state">{STATE[a.integration.global_hooks]}</dd>
              <dt>MCP server</dt>
              <dd class={a.integration.mcp} data-testid="mcp-state">{STATE[a.integration.mcp]}</dd>
              <dt>Memory at launch</dt>
              <dd>{INJECT[a.integration.inject]}</dd>
              {#if a.token}
                <dt>Login</dt>
                <dd data-testid="login-state">{loginText(a)}</dd>
                <dt>Login token</dt>
                <dd data-testid="token-state">{tokenText(a.token)}</dd>
              {/if}
            </dl>
            {#if a.integration.detail}<span class="small muted">{a.integration.detail}</span>{/if}
            {#if a.token}
              <p class="small muted token-help">
                A hub without a desktop login (a LaunchAgent on a locked Mac, a daemon started over SSH) cannot reach the keychain,
                so Claude Code hangs or reports it is logged out. Give it a long-lived login token instead: run
                <code>claude setup-token</code> on any machine with a browser, then on this machine
                <code>blirp agents set-token claude</code> and paste the token{app.admin ? ', or paste it below' : ''}. It applies to
                this machine only and is never synced.
              </p>
              {#if app.admin}
                <form class="row wrap" onsubmit={saveToken}>
                  <input
                    class="input mono grow"
                    type="password"
                    aria-label="Claude login token"
                    placeholder={a.token.stored ? 'Paste a new token to replace the stored one' : 'Token from claude setup-token'}
                    bind:value={tokenInput}
                    autocomplete="off"
                    spellcheck="false"
                  />
                  <button type="submit" class="btn sm primary" disabled={savingToken || tokenInput.trim() === ''}>Save token</button>
                  {#if a.token.stored}
                    <button type="button" class="btn sm" disabled={savingToken} onclick={clearToken}>Remove token</button>
                  {/if}
                </form>
              {/if}
            {/if}
          </div>
          {#if app.admin && supported(a)}
            <div class="acts">
              {#if anyIs(a, 'not_installed')}
                <button type="button" class="btn sm primary" disabled={busyAgent === a.id} onclick={() => (confirming = a)}>
                  {anyIs(a, 'installed') ? 'Complete install' : 'Install'}
                </button>
              {/if}
              {#if anyIs(a, 'installed')}
                <button type="button" class="btn sm" disabled={busyAgent === a.id} onclick={() => uninstall(a)}>Uninstall</button>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<Modal open={confirming !== null} title="Install global integration" onclose={() => (confirming = null)}>
  {#if confirming}
    <p>
      This edits {nameOf(confirming)}'s user configuration on this machine, so sessions started outside blirp report to blirp and
      receive its memory:
    </p>
    <ul class="files">
      {#each FILES[confirming.id] ?? ["the agent's user config"] as f (f)}<li class="mono small">{f}</li>{/each}
    </ul>
    <p class="small muted">
      Existing entries are kept and each file is backed up once to <code>&lt;file&gt;.blirp-backup</code>. Files with comments or
      invalid syntax are never rewritten. Uninstall removes exactly the added entries.
    </p>
  {/if}
  {#snippet footer()}
    <button type="button" class="btn" onclick={() => (confirming = null)}>Cancel</button>
    <button type="button" class="btn primary" onclick={() => confirming && run(confirming, true)}>Install</button>
  {/snippet}
</Modal>

<section class="card panel-pad">
  <h2 class="h">Defaults</h2>
  <label class="field top">
    <span>Default agent for new sessions</span>
    <select
      class="select narrow"
      disabled={!app.admin}
      value={settings.config.agents.default}
      onchange={(e) => {
        const value = e.currentTarget.value;
        void saveConfig((c) => ({ ...c, agents: { ...c.agents, default: value } }));
      }}
    >
      {#each app.agents.filter((a) => a.installed || a.id === settings.config.agents.default) as a (a.id)}
        <option value={a.id}>{nameOf(a)}</option>
      {/each}
    </select>
  </label>
  <label class="check">
    <input
      type="checkbox"
      disabled={!app.admin}
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
  .integ {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 2px 12px;
    margin: 4px 0 0;
  }
  .integ dt {
    color: var(--text-2);
  }
  .integ dd {
    margin: 0;
  }
  .integ .installed {
    color: var(--completed);
  }
  .integ .unsupported {
    color: var(--text-3);
  }
  .acts {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .token-help {
    margin: 6px 0 0;
  }
  .grow {
    flex: 1;
    min-width: 200px;
  }
  .files {
    margin: 8px 0;
    padding-left: 18px;
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
