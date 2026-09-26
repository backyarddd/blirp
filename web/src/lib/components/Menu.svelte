<script lang="ts" module>
  export interface MenuItem {
    label: string;
    hint?: string;
    disabled?: boolean;
    onselect: () => void;
  }
</script>

<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    items: MenuItem[];
    /** Accessible name of the trigger button. */
    label: string;
    heading?: string;
    triggerClass?: string;
    align?: 'left' | 'right';
    disabled?: boolean;
    children: Snippet;
  }

  let { items, label, heading, triggerClass = 'btn sm', align = 'left', disabled = false, children }: Props = $props();

  const id = `menu-${Math.random().toString(36).slice(2)}`;
  let trigger: HTMLButtonElement | undefined = $state();
  let menu: HTMLDivElement | undefined = $state();

  // Native popover: top layer, light dismiss and Esc for free. Position it under the trigger.
  $effect(() => {
    const m = menu;
    if (!m) return;
    const onBefore = (e: Event): void => {
      if (!(e instanceof ToggleEvent) || e.newState !== 'open' || !trigger) return;
      const r = trigger.getBoundingClientRect();
      m.style.top = `${Math.round(r.bottom + 4)}px`;
      if (align === 'right') {
        m.style.left = 'auto';
        m.style.right = `${Math.round(window.innerWidth - r.right)}px`;
      } else {
        m.style.right = 'auto';
        m.style.left = `${Math.round(r.left)}px`;
      }
    };
    const onToggle = (e: Event): void => {
      if (e instanceof ToggleEvent && e.newState === 'open') m.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
    };
    m.addEventListener('beforetoggle', onBefore);
    m.addEventListener('toggle', onToggle);
    return () => {
      m.removeEventListener('beforetoggle', onBefore);
      m.removeEventListener('toggle', onToggle);
    };
  });

  function onKey(e: KeyboardEvent): void {
    if (!menu || (e.key !== 'ArrowDown' && e.key !== 'ArrowUp')) return;
    e.preventDefault();
    const buttons = [...menu.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
    const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = buttons[(i + (e.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length];
    next?.focus();
  }

  function select(item: MenuItem): void {
    menu?.hidePopover();
    item.onselect();
  }
</script>

<button
  bind:this={trigger}
  type="button"
  class={triggerClass}
  aria-label={label}
  aria-haspopup="menu"
  popovertarget={id}
  {disabled}>{@render children()}</button
>
<div bind:this={menu} {id} popover="auto" class="menu" role="menu" tabindex="-1" onkeydown={onKey}>
  {#if heading}<div class="heading">{heading}</div>{/if}
  {#each items as item (item.label)}
    <button type="button" role="menuitem" disabled={item.disabled} onclick={() => select(item)}>
      <span>{item.label}</span>
      {#if item.hint}<span class="hint">{item.hint}</span>{/if}
    </button>
  {:else}
    <div class="heading">No options</div>
  {/each}
</div>

<style>
  .menu {
    position: fixed;
    inset: auto;
    margin: 0;
    min-width: 200px;
    max-width: min(320px, calc(100vw - 16px));
    padding: 4px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    color: var(--text);
    box-shadow: var(--shadow-lg);
  }
  .heading {
    padding: 6px 10px 4px;
    font-size: 11.5px;
    font-weight: 700;
    color: var(--text-3);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .menu button {
    display: flex;
    width: 100%;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 7px 10px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    text-align: left;
    cursor: pointer;
  }
  .menu button:hover:not(:disabled),
  .menu button:focus-visible {
    background: var(--panel-2);
  }
  .menu button:disabled {
    color: var(--text-3);
    cursor: not-allowed;
  }
</style>
