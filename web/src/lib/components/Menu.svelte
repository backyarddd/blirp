<script lang="ts" module>
  export type { MenuItem } from '../menu';
</script>

<script lang="ts">
  import type { Snippet } from 'svelte';
  import { isSeparator, tidy, type MenuAction, type MenuItem } from '../menu';

  interface Props {
    items: MenuItem[];
    /** Accessible name of the trigger button, and of the menu. */
    label: string;
    heading?: string;
    triggerClass?: string;
    /** Tooltip of the trigger button. */
    title?: string;
    align?: 'left' | 'right';
    disabled?: boolean;
    /** The trigger's content. Without it the menu has no trigger and opens with `openAt`. */
    children?: Snippet;
  }

  let { items, label, heading, triggerClass = 'btn sm', title, align = 'left', disabled = false, children }: Props = $props();

  const id = `menu-${Math.random().toString(36).slice(2)}`;
  let trigger: HTMLButtonElement | undefined = $state();
  let menu: HTMLDivElement | undefined = $state();
  /** Where focus goes back to when the menu closes (a context menu's item). */
  let returnTo: HTMLElement | null = null;
  // A trigger menu renders its items only while open: lists show one per row.
  let open = $state(false);
  const shown = $derived(tidy(items));

  const MARGIN = 8;

  /** Keep the open menu inside the viewport. */
  function clamp(m: HTMLElement): void {
    const r = m.getBoundingClientRect();
    const left = Math.min(Math.max(MARGIN, r.left), window.innerWidth - r.width - MARGIN);
    const top = r.bottom > window.innerHeight - MARGIN ? Math.max(MARGIN, r.top - r.height - (trigger ? trigger.offsetHeight + 8 : 0)) : r.top;
    m.style.right = 'auto';
    m.style.left = `${Math.round(Math.max(MARGIN, left))}px`;
    m.style.top = `${Math.round(top)}px`;
  }

  function focusItem(which: 'first' | 'last'): void {
    const buttons = menu ? [...menu.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')] : [];
    (which === 'first' ? buttons[0] : buttons.at(-1))?.focus();
  }

  /** Open at a point (a right-click) without a trigger; focus returns to `from` when it closes. */
  export function openAt(x: number, y: number, from: HTMLElement | null): void {
    const m = menu;
    if (!m) return;
    if (m.matches(':popover-open')) m.hidePopover();
    returnTo = from;
    m.style.right = 'auto';
    m.style.left = `${Math.round(x)}px`;
    m.style.top = `${Math.round(y)}px`;
    m.showPopover();
    clamp(m);
  }

  // Native popover: top layer, light dismiss and Esc for free. With a trigger it opens under it.
  $effect(() => {
    const m = menu;
    if (!m) return;
    const onBefore = (e: Event): void => {
      if (!(e instanceof ToggleEvent)) return;
      open = e.newState === 'open';
      if (!open || !trigger) return;
      returnTo = null;
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
      if (!(e instanceof ToggleEvent)) return;
      if (e.newState === 'open') {
        if (trigger) clamp(m);
        focusItem('first');
        return;
      }
      // Closed without a choice that moved focus elsewhere (Esc, a click outside, Tab).
      const active = document.activeElement;
      const target = returnTo;
      returnTo = null;
      if (target?.isConnected && (active === null || active === document.body || m.contains(active))) target.focus();
    };
    m.addEventListener('beforetoggle', onBefore);
    m.addEventListener('toggle', onToggle);
    return () => {
      m.removeEventListener('beforetoggle', onBefore);
      m.removeEventListener('toggle', onToggle);
    };
  });

  function onKey(e: KeyboardEvent): void {
    if (!menu) return;
    if (e.key === 'Tab') {
      // Menus are not in the tab order: Tab leaves, like Esc.
      menu.hidePopover();
      return;
    }
    if (e.key === 'Home' || e.key === 'End') {
      e.preventDefault();
      focusItem(e.key === 'Home' ? 'first' : 'last');
      return;
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const buttons = [...menu.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
    const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = buttons[(i + (e.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length];
    next?.focus();
  }

  function select(item: MenuAction): void {
    menu?.hidePopover();
    item.onselect();
  }
</script>

{#if children}
  <button
    bind:this={trigger}
    type="button"
    class={triggerClass}
    aria-label={label}
    {title}
    aria-haspopup="menu"
    popovertarget={id}
    {disabled}>{@render children()}</button
  >
{/if}
<div bind:this={menu} {id} popover="auto" class="menu" role="menu" aria-label={label} tabindex="-1" onkeydown={onKey}>
  {#if heading}<div class="heading">{heading}</div>{/if}
  <!-- Unkeyed: labels are not unique (a custom agent may be named like a built-in one). -->
  {#each open || !children ? shown : [] as item}
    {#if isSeparator(item)}
      <div class="sep" role="separator"></div>
    {:else}
      {@const Icon = item.icon}
      <button type="button" role="menuitem" class:danger={item.danger} disabled={item.disabled} onclick={() => select(item)}>
        <span class="label">
          {#if Icon}<Icon size={15} />{/if}
          <span>{item.label}</span>
        </span>
        {#if item.hint}<span class="hint">{item.hint}</span>{/if}
      </button>
    {/if}
  {:else}
    {#if open || !children}<div class="heading">No options</div>{/if}
  {/each}
</div>

<style>
  .menu {
    position: fixed;
    inset: auto;
    margin: 0;
    min-width: 200px;
    max-width: min(320px, calc(100vw - 16px));
    max-height: calc(100vh - 16px);
    overflow: auto;
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
  .sep {
    height: 1px;
    margin: 4px 6px;
    background: var(--border);
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
  .label {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .label :global(svg) {
    flex: none;
    color: var(--text-2);
  }
  .menu button.danger,
  .menu button.danger .label :global(svg) {
    color: var(--danger);
  }
  .menu button:hover:not(:disabled),
  .menu button:focus-visible {
    background: var(--panel-2);
  }
  .menu button:disabled {
    color: var(--text-3);
    cursor: not-allowed;
  }
  .hint {
    font-size: 12px;
    color: var(--text-3);
    white-space: nowrap;
  }
</style>
