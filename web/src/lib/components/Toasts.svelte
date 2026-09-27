<script lang="ts">
  import X from '@lucide/svelte/icons/x';
  import { app } from '../app.svelte';
</script>

<div class="toasts" aria-live="polite">
  {#each app.toasts as t (t.id)}
    <div class="toast {t.kind}" role={t.kind === 'error' ? 'alert' : 'status'}>
      <span>{t.text}</span>
      {#if t.action}
        {@const action = t.action}
        <button
          type="button"
          class="btn sm"
          onclick={() => {
            app.dismissToast(t.id);
            action.run();
          }}>{action.label}</button
        >
      {/if}
      <button type="button" class="icon-btn sm" aria-label="Dismiss" onclick={() => app.dismissToast(t.id)}><X size={14} /></button>
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    right: 16px;
    bottom: 16px;
    display: grid;
    gap: 8px;
    z-index: 100;
    max-width: min(420px, calc(100vw - 32px));
  }
  .toast {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 10px 8px 10px 14px;
    border-radius: 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-left: 4px solid var(--completed);
    box-shadow: var(--shadow-lg);
  }
  .toast.error {
    border-left-color: var(--danger);
  }
  .toast span {
    flex: 1;
    padding-top: 2px;
    overflow-wrap: anywhere;
  }
</style>
