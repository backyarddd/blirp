<script lang="ts">
  import ArrowUp from '@lucide/svelte/icons/arrow-up';
  import ArrowDown from '@lucide/svelte/icons/arrow-down';
  import CaseSensitive from '@lucide/svelte/icons/case-sensitive';
  import WholeWord from '@lucide/svelte/icons/whole-word';
  import Regex from '@lucide/svelte/icons/regex';
  import X from '@lucide/svelte/icons/x';
  import { untrack } from 'svelte';
  import type { ISearchOptions, SearchAddon } from '@xterm/addon-search';
  import { theme } from '../theme.svelte';

  // VS Code's terminal find widget: typing searches upwards from the bottom (newest output first),
  // Enter finds the previous (older) match and Shift+Enter the next, F3 / Shift+F3 too, Alt+C/W/R
  // (Cmd+Alt on macOS) toggle case, whole word and regex, Escape closes and refocuses the terminal.
  interface Props {
    search: SearchAddon;
    initial: string;
    mac: boolean;
    onclose: () => void;
  }

  let { search, initial, mac, onclose }: Props = $props();

  let input: HTMLInputElement | undefined = $state();
  let term = $state(untrack(() => initial));
  let caseSensitive = $state(false);
  let wholeWord = $state(false);
  let regex = $state(false);
  let results = $state<{ index: number; count: number } | null>(null);

  $effect(() => {
    const sub = search.onDidChangeResults((r) => {
      results = { index: r.resultIndex, count: r.resultCount };
    });
    return () => sub.dispose();
  });

  $effect(() => {
    input?.focus();
    input?.select();
  });

  // Match colors must be #RRGGBB; VS Code's terminal find colors.
  const options = (incremental: boolean): ISearchOptions => ({
    caseSensitive,
    wholeWord,
    regex,
    incremental,
    decorations:
      theme.resolved === 'dark'
        ? {
            matchBackground: '#623315',
            activeMatchBackground: '#515c6a',
            matchOverviewRuler: '#d18616',
            activeMatchColorOverviewRuler: '#a0a0a0',
          }
        : {
            matchBackground: '#f5d0b5',
            activeMatchBackground: '#a8ac94',
            matchOverviewRuler: '#d18616',
            activeMatchColorOverviewRuler: '#a0a0a0',
          },
  });

  function find(previous: boolean, incremental = false): void {
    if (term === '') {
      search.clearDecorations();
      results = null;
      return;
    }
    try {
      if (previous) search.findPrevious(term, options(incremental));
      else search.findNext(term, options(incremental));
    } catch {
      // An incomplete regular expression while typing: no matches yet.
      results = { index: -1, count: 0 };
    }
  }

  // Typing and toggles search again from the newest output.
  $effect(() => {
    void [term, caseSensitive, wholeWord, regex, theme.resolved];
    find(true, true);
  });

  function close(): void {
    search.clearDecorations();
    onclose();
  }

  function onKey(e: KeyboardEvent): void {
    const toggle = mac ? e.metaKey && e.altKey : e.altKey && !e.ctrlKey && !e.metaKey;
    const key = e.key.toLowerCase();
    if (e.key === 'Escape') close();
    else if (e.key === 'Enter' || e.key === 'F3') find(e.key === 'Enter' ? !e.shiftKey : e.shiftKey);
    else if (toggle && (key === 'c' || e.code === 'KeyC')) caseSensitive = !caseSensitive;
    else if (toggle && (key === 'w' || e.code === 'KeyW')) wholeWord = !wholeWord;
    else if (toggle && (key === 'r' || e.code === 'KeyR')) regex = !regex;
    else return;
    e.preventDefault();
    e.stopPropagation();
  }

  const status = $derived(
    term === '' || results === null
      ? ''
      : results.count === 0
        ? 'No results'
        : results.index < 0
          ? `${results.count}+ results`
          : `${results.index + 1} of ${results.count}`,
  );
</script>

<div class="find" role="search" aria-label="Find in terminal" data-testid="terminal-find">
  <input
    bind:this={input}
    bind:value={term}
    type="text"
    placeholder="Find"
    aria-label="Find"
    spellcheck="false"
    autocomplete="off"
    onkeydown={onKey}
  />
  <button type="button" class="icon-btn sm" aria-label="Match case" title="Match case" aria-pressed={caseSensitive} onclick={() => (caseSensitive = !caseSensitive)}>
    <CaseSensitive size={16} />
  </button>
  <button type="button" class="icon-btn sm" aria-label="Match whole word" title="Match whole word" aria-pressed={wholeWord} onclick={() => (wholeWord = !wholeWord)}>
    <WholeWord size={16} />
  </button>
  <button type="button" class="icon-btn sm" aria-label="Use regular expression" title="Use regular expression" aria-pressed={regex} onclick={() => (regex = !regex)}>
    <Regex size={16} />
  </button>
  <span class="status" role="status" aria-live="polite">{status}</span>
  <button type="button" class="icon-btn sm" aria-label="Previous match" title="Previous match (Enter)" onclick={() => find(true)}>
    <ArrowUp size={16} />
  </button>
  <button type="button" class="icon-btn sm" aria-label="Next match" title="Next match (Shift+Enter)" onclick={() => find(false)}>
    <ArrowDown size={16} />
  </button>
  <button type="button" class="icon-btn sm" aria-label="Close find" title="Close (Escape)" onclick={close}>
    <X size={16} />
  </button>
</div>

<style>
  .find {
    position: absolute;
    top: 6px;
    right: 18px;
    z-index: 6;
    display: flex;
    align-items: center;
    gap: 2px;
    max-width: calc(100% - 24px);
    padding: 4px 6px;
    border-radius: 8px;
    background: var(--panel);
    border: 1px solid var(--border);
    box-shadow: var(--shadow-lg);
  }
  input {
    width: 180px;
    min-width: 60px;
    flex: 1 1 auto;
    font: inherit;
    font-size: 13px;
    padding: 3px 6px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--panel-2);
    color: var(--text);
  }
  [aria-pressed='true'] {
    color: var(--accent);
    background: var(--panel-2);
  }
  .status {
    min-width: 64px;
    padding: 0 6px;
    font-size: 12px;
    color: var(--text-2);
    white-space: nowrap;
  }
</style>
