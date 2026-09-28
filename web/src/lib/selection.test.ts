import { describe, expect, it } from 'vitest';
import { Selection, rangeBetween } from './selection.svelte';

describe('selection', () => {
  it('selects ranges in list order, either direction', () => {
    const order = ['a', 'b', 'c', 'd'];
    expect(rangeBetween(order, 'b', 'd')).toEqual(['b', 'c', 'd']);
    expect(rangeBetween(order, 'd', 'a')).toEqual(['a', 'b', 'c', 'd']);
    expect(rangeBetween(order, null, 'c')).toEqual(['c']);
    expect(rangeBetween(order, 'gone', 'c')).toEqual(['c']);
  });

  it('toggles, shift-selects from the last click, selects all and clears', () => {
    const order = ['a', 'b', 'c', 'd'];
    const sel = new Selection();
    sel.toggle('a', false, order);
    sel.toggle('c', true, order);
    expect([...sel.ids].sort()).toEqual(['a', 'b', 'c']);
    // A Shift+click on a selected item clears the range.
    sel.toggle('b', true, order);
    expect([...sel.ids]).toEqual(['c']);
    sel.toggleAll(order);
    expect(sel.ids.size).toBe(4);
    sel.toggleAll(order);
    expect(sel.ids.size).toBe(0);
    sel.toggleAll(order);
    sel.retain(['a', 'd']);
    expect([...sel.ids].sort()).toEqual(['a', 'd']);
    sel.start();
    sel.stop();
    expect(sel.active).toBe(false);
    expect(sel.ids.size).toBe(0);
  });
});
