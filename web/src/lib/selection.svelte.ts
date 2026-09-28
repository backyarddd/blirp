// Selection mode of a session list (bulk actions): checkboxes, Shift+click for a range, select all.
import { SvelteSet } from 'svelte/reactivity';

/** Ids from `a` to `b` (either order) in `order`; just `b` when `a` is not shown there. */
export function rangeBetween(order: readonly string[], a: string | null, b: string): string[] {
  const j = order.indexOf(b);
  const i = a === null ? -1 : order.indexOf(a);
  if (i < 0 || j < 0) return [b];
  return order.slice(Math.min(i, j), Math.max(i, j) + 1);
}

export class Selection {
  /** Selection mode is on (the list shows checkboxes). */
  active = $state(false);
  readonly ids = new SvelteSet<string>();
  /** The last item clicked without Shift: a Shift+click selects from here. */
  #anchor: string | null = null;

  /** A click on `id`'s checkbox; with Shift, the range from the last click takes its new state. */
  toggle(id: string, shift: boolean, order: readonly string[]): void {
    const on = !this.ids.has(id);
    const ids = shift ? rangeBetween(order, this.#anchor, id) : [id];
    for (const x of ids) {
      if (on) this.ids.add(x);
      else this.ids.delete(x);
    }
    if (!shift) this.#anchor = id;
  }

  /** Select every shown item, or clear them all when they are all selected. */
  toggleAll(order: readonly string[]): void {
    if (order.length > 0 && order.every((id) => this.ids.has(id))) this.ids.clear();
    else for (const id of order) this.ids.add(id);
  }

  /** Keep only ids still shown (after a delete, a filter). */
  retain(order: readonly string[]): void {
    const shown = new Set(order);
    for (const id of [...this.ids]) if (!shown.has(id)) this.ids.delete(id);
  }

  start(): void {
    this.active = true;
  }

  stop(): void {
    this.active = false;
    this.ids.clear();
    this.#anchor = null;
  }
}
