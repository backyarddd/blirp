<script lang="ts">
  import type { Snippet } from 'svelte';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';

  interface Props {
    loading: boolean;
    error: string | null;
    empty?: boolean;
    emptyText?: string;
    emptyAction?: Snippet;
    onretry?: () => void;
    children: Snippet;
  }

  let { loading, error, empty = false, emptyText = 'Nothing here yet.', emptyAction, onretry, children }: Props = $props();
</script>

{#if error}
  <div class="state error" role="alert">
    <CircleAlert size={18} aria-hidden="true" />
    <span>{error}</span>
    {#if onretry}<button class="btn sm" type="button" onclick={onretry}>Retry</button>{/if}
  </div>
{:else if loading && empty}
  <div class="state" role="status" aria-live="polite"><span class="spinner" aria-hidden="true"></span>Loading…</div>
{:else if empty}
  <div class="state empty">
    <span>{emptyText}</span>
    {#if emptyAction}{@render emptyAction()}{/if}
  </div>
{:else}
  {@render children()}
{/if}

<style>
  .state {
    display: flex;
    align-items: center;
    justify-content: center;
    flex-wrap: wrap;
    gap: 10px;
    padding: 28px 16px;
    color: var(--text-2);
    text-align: center;
  }
  .state.empty {
    flex-direction: column;
  }
  .state.error {
    color: var(--danger);
  }
  .spinner {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    animation: spin 0.8s linear infinite;
  }
</style>
