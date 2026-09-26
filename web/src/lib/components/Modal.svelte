<script lang="ts">
  import type { Snippet } from 'svelte';
  import X from '@lucide/svelte/icons/x';

  interface Props {
    open: boolean;
    title: string;
    wide?: boolean;
    onclose: () => void;
    children: Snippet;
    footer?: Snippet;
  }

  let { open, title, wide = false, onclose, children, footer }: Props = $props();
  let dialog: HTMLDialogElement | undefined = $state();
  const titleId = `modal-${Math.random().toString(36).slice(2)}`;

  // Native modal <dialog>: the rest of the page is inert (focus stays inside) and Esc closes.
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) dialog.close();
  });
</script>

<dialog
  bind:this={dialog}
  class="modal"
  class:wide
  aria-labelledby={titleId}
  onclose={() => {
    if (open) onclose();
  }}
  onclick={(e) => {
    if (e.target === dialog) onclose();
  }}
>
  {#if open}
    <div class="head">
      <h2 id={titleId}>{title}</h2>
      <button type="button" class="icon-btn" aria-label="Close" onclick={onclose}><X size={18} /></button>
    </div>
    <div class="body">{@render children()}</div>
    {#if footer}<div class="foot">{@render footer()}</div>{/if}
  {/if}
</dialog>

<style>
  .wide {
    width: min(820px, calc(100vw - 24px));
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 16px 6px 20px;
  }
  h2 {
    font-size: 16px;
    margin: 0;
  }
  .body {
    padding: 8px 20px 16px;
  }
  .foot {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 20px 16px;
    border-top: 1px solid var(--border);
  }
</style>
