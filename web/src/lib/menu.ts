// Menu items shared by the trigger menus, the context menus and the command palette.
import type { Component } from 'svelte';

export interface MenuAction {
  label: string;
  /** Shown at the end of the row: a shortcut (F2) or why the item is disabled. */
  hint?: string;
  disabled?: boolean;
  /** Destructive: shown in the danger color. */
  danger?: boolean;
  icon?: Component<{ size?: number }>;
  onselect: () => void;
}

export interface MenuSeparator {
  separator: true;
}

export type MenuItem = MenuAction | MenuSeparator;

export const SEPARATOR: MenuSeparator = { separator: true };

export function isSeparator(item: MenuItem): item is MenuSeparator {
  return 'separator' in item;
}

/** Without separators at either end or next to each other (gated items leave such gaps). */
export function tidy(items: readonly MenuItem[]): MenuItem[] {
  const out: MenuItem[] = [];
  const endsInSeparator = (): boolean => {
    const last = out.at(-1);
    return last === undefined || isSeparator(last);
  };
  for (const it of items) {
    if (isSeparator(it) && endsInSeparator()) continue;
    out.push(it);
  }
  if (out.length > 0 && endsInSeparator()) out.pop();
  return out;
}

/** The actions of a menu, for lists that show no separators (the command palette). */
export function actionsOf(items: readonly MenuItem[]): MenuAction[] {
  return items.filter((i): i is MenuAction => !isSeparator(i));
}
