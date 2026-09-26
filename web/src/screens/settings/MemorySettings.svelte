<script lang="ts">
  import { untrack } from 'svelte';
  import { api } from '../../lib/api/client';
  import type { BriefMode, Settings, Summarizer } from '../../lib/api/types';
  import { app } from '../../lib/app.svelte';

  let { settings, onsaved }: { settings: Settings; onsaved: (s: Settings) => void } = $props();

  // Local draft, initialised once; saving replaces the parent's settings.
  let form: Settings['memory'] = $state(untrack(() => ({ ...settings.memory })));
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
    const s = await app.act(() => api.settings.patch({ memory: { ...form, ollama_model: form.ollama_model.trim() } }), 'Memory settings saved');
    saving = false;
    if (s) onsaved(s);
  }
</script>

<form class="card panel-pad" onsubmit={save}>
  <h2 class="h">Memory</h2>
  <p class="hint">blirp summarizes finished or idle sessions into decisions, open threads and a project brief, then injects that into new sessions.</p>

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
  <div class="row">
    <span class="spacer"></span>
    <button type="submit" class="btn primary" disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
  </div>
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
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 0 16px;
  }
  .err {
    color: var(--danger);
  }
</style>
