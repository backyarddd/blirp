<script lang="ts">
  import { untrack } from 'svelte';
  import type { BriefMode, DistillPause, MemoryConfig, SettingsView, Summarizer } from '../../lib/api/types.gen';
  import { app } from '../../lib/app.svelte';
  import { agentLabel } from '../../lib/status';
  import { formatElapsed, formatTime } from '../../lib/time';

  let { settings, onsaved }: { settings: SettingsView; onsaved: (s: SettingsView) => void } = $props();

  // Local draft, initialised once; saving replaces the parent's settings.
  let form: MemoryConfig = $state(
    untrack(() => ({ ...settings.config.memory, inject_disabled_agents: [...settings.config.memory.inject_disabled_agents] })),
  );
  let saving = $state(false);
  let formError: string | null = $state(null);

  const SUMMARIZERS: { id: Summarizer; label: string }[] = [
    { id: 'auto', label: 'Automatic (first available)' },
    { id: 'claude', label: 'Claude Code (haiku)' },
    { id: 'codex', label: 'Codex' },
    { id: 'ollama', label: 'Ollama (local model)' },
    { id: 'none', label: 'Off (no distilling)' },
  ];
  const MODES: { id: BriefMode; label: string }[] = [
    { id: 'auto', label: 'Update the brief automatically (history kept)' },
    { id: 'review', label: 'Propose changes as suggestions for me to review' },
  ];

  const INTS: { key: 'distill_idle_secs' | 'daily_distill_limit' | 'inject_max_chars' | 'distill_max_chars'; label: string; hint: string; min: number }[] = [
    { key: 'distill_idle_secs', label: 'Distill after idle (seconds)', hint: 'How long a session sits idle before it is summarized.', min: 30 },
    { key: 'daily_distill_limit', label: 'Daily distill limit', hint: 'Caps summarizer runs per day.', min: 0 },
    { key: 'inject_max_chars', label: 'Injected memory size (characters)', hint: 'Hard cap on memory added to each new session.', min: 500 },
    { key: 'distill_max_chars', label: 'Transcript size sent to the summarizer (characters)', hint: 'Longer transcripts keep the head and tail.', min: 2000 },
  ];

  // Detected agents plus any configured id that is not detected here (kept, never dropped).
  const injectAgents = $derived([
    ...app.agents.map((a) => ({ id: a.id, label: a.display_name || agentLabel(a.id) })),
    ...form.inject_disabled_agents.filter((id) => !app.agents.some((a) => a.id === id)).map((id) => ({ id, label: agentLabel(id) })),
  ]);

  function setInjectFor(id: string, on: boolean): void {
    const rest = form.inject_disabled_agents.filter((x) => x !== id);
    form.inject_disabled_agents = on ? rest : [...rest, id];
  }

  const PAUSES: Record<DistillPause, string> = {
    auth: 'the summarizer is not signed in or its key is invalid',
    unavailable: 'the summarizer is not installed or not reachable',
    rate_limited: 'the summarizer hit a rate or usage limit',
  };
  const distill = $derived(settings.distill);
  let now = $state(Date.now());
  $effect(() => {
    if (!distill.paused) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    formError = null;
    for (const f of INTS) {
      const v = form[f.key];
      if (!Number.isInteger(v) || v < f.min) {
        formError = `${f.label} must be a whole number of at least ${f.min}.`;
        return;
      }
    }
    if (form.summarizer === 'ollama' && !form.ollama_model.trim()) {
      formError = 'Pick an Ollama model.';
      return;
    }
    saving = true;
    const s = await app.saveSettings(
      { config: { ...settings.config, memory: { ...form, ollama_model: form.ollama_model.trim() } } },
      'Memory settings saved',
    );
    saving = false;
    if (s) onsaved(s);
  }
</script>

<form class="card panel-pad" onsubmit={save}>
  <h2 class="h">Memory</h2>
  <p class="hint">blirp summarizes finished or idle sessions into decisions, open threads and a project brief, then injects that into new sessions.</p>
  {#if !app.admin}
    <p class="small muted">Settings can only be changed from this machine's own desktop app or CLI.</p>
  {/if}

  <div class="status" role="status" data-testid="distill-status">
    {#if distill.paused}
      <p class="warn">
        <strong>Automatic distilling is paused</strong>: {PAUSES[distill.paused]}.
        {#if distill.retry_at}
          blirp tries again at {formatTime(distill.retry_at)}{distill.retry_at > now ? ` (in ${formatElapsed(distill.retry_at - now)})` : ''}.
        {/if}
      </p>
      {#if distill.reason}<p class="mono small reason">{distill.reason}</p>{/if}
      <p class="small muted">
        It resumes by itself: each retry waits longer (up to 6 hours) and the first success resumes distilling. Fix the cause (sign
        in, install, or wait for the limit) to speed that up; "Distill now" on a session always tries right away.
      </p>
    {:else}
      <p class="small muted">Automatic distilling is running.</p>
    {/if}
    <p class="small muted">Distill jobs today: {distill.budget_used} of {distill.budget_limit}.</p>
  </div>
<fieldset class="bare" disabled={!app.admin}>

  <label class="field">
    <span>Summarizer</span>
    <select class="select" bind:value={form.summarizer}>
      {#each SUMMARIZERS as s (s.id)}<option value={s.id}>{s.label}</option>{/each}
    </select>
  </label>
  {#if form.summarizer === 'ollama' || form.summarizer === 'auto'}
    <label class="field">
      <span>Ollama model</span>
      <input class="input mono" bind:value={form.ollama_model} placeholder="qwen2.5:7b" />
    </label>
  {/if}

  <fieldset class="field">
    <legend class="label">Brief updates</legend>
    {#each MODES as m (m.id)}
      <label class="check"><input type="radio" name="brief_mode" value={m.id} bind:group={form.brief_mode} />{m.label}</label>
    {/each}
  </fieldset>

  <fieldset class="field">
    <legend class="label">Memory injection</legend>
    <label class="check"><input type="checkbox" bind:checked={form.inject} />Give new sessions this project's memory when they start</label>
    <span class="hint">Off: sessions start without memory. The MCP tools and <code>blirp mem</code> still reach it on demand.</span>
    {#if form.inject && injectAgents.length > 0}
      <div class="agents" role="group" aria-label="Inject memory for these agents">
        {#each injectAgents as a (a.id)}
          <label class="check small">
            <input
              type="checkbox"
              checked={!form.inject_disabled_agents.includes(a.id)}
              onchange={(e) => setInjectFor(a.id, e.currentTarget.checked)}
            />{a.label}
          </label>
        {/each}
      </div>
      <span class="hint">Unchecked agents never get memory injected.</span>
    {/if}
  </fieldset>

  <div class="grid">
    {#each INTS as f (f.key)}
      <label class="field">
        <span>{f.label}</span>
        <input class="input" type="number" min={f.min} step="1" bind:value={form[f.key]} required />
        <span class="hint">{f.hint}</span>
      </label>
    {/each}
  </div>

  {#if formError}<p class="err" role="alert">{formError}</p>{/if}
  {#if app.admin}
    <div class="row">
      <span class="spacer"></span>
      <button type="submit" class="btn primary" disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
    </div>
  {/if}
</fieldset>
</form>

<style>
  .h {
    font-size: 15px;
    margin: 0 0 4px;
  }
  .hint {
    margin-top: 0;
  }
  fieldset {
    border: 0;
    padding: 0;
    margin: 0 0 12px;
    display: grid;
    gap: 6px;
  }
  .bare {
    display: block;
    margin: 0;
  }
  .status {
    margin: 0 0 16px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel-2);
  }
  .status p {
    margin: 0 0 4px;
  }
  .warn {
    color: var(--waiting);
  }
  .reason {
    overflow-wrap: anywhere;
    color: var(--text-2);
  }
  .agents {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 16px;
    margin-left: 24px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 0 16px;
  }
  .err {
    color: var(--danger);
  }
</style>
