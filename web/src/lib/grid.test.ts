import { describe, expect, it } from 'vitest';
import { gridLayout, moveTile, orderTiles } from './grid';

describe('orderTiles', () => {
  const s = (id: string, started_at: number): { id: string; started_at: number } => ({ id, started_at });

  it('keeps saved order, then appends the rest oldest first', () => {
    const tiles = orderTiles([s('new', 30), s('b', 10), s('a', 20), s('old', 5)], ['a', 'gone', 'b']);
    expect(tiles.map((t) => t.id)).toEqual(['a', 'b', 'old', 'new']);
  });

  it('orders by start time without a saved order', () => {
    expect(orderTiles([s('b', 2), s('a', 1)], []).map((t) => t.id)).toEqual(['a', 'b']);
  });
});

describe('moveTile', () => {
  it('moves forward and back', () => {
    expect(moveTile(['a', 'b', 'c', 'd'], 0, 2)).toEqual(['b', 'c', 'a', 'd']);
    expect(moveTile(['a', 'b', 'c', 'd'], 3, 0)).toEqual(['d', 'a', 'b', 'c']);
  });

  it('clamps the target', () => {
    expect(moveTile(['a', 'b'], 0, 9)).toEqual(['b', 'a']);
  });
});

describe('gridLayout', () => {
  it('tiles four as 2x2 and nine as 3x3', () => {
    expect(gridLayout(4, 4)).toEqual({ columns: 2, span: 1, lastSpan: 1, lastFrom: 4 });
    expect(gridLayout(9, 4)).toEqual({ columns: 3, span: 1, lastSpan: 1, lastFrom: 9 });
  });

  it('stretches a short last row across the width', () => {
    // 3 tiles: 2 on top, 1 full width below.
    expect(gridLayout(3, 4)).toEqual({ columns: 2, span: 1, lastSpan: 2, lastFrom: 2 });
    // 5 tiles: 3 on top, 2 each half wide below (6 columns: 2 + 2 + 2, then 3 + 3).
    expect(gridLayout(5, 4)).toEqual({ columns: 6, span: 2, lastSpan: 3, lastFrom: 3 });
  });

  it('stacks when the width fits one column', () => {
    expect(gridLayout(4, 1)).toEqual({ columns: 1, span: 1, lastSpan: 1, lastFrom: 4 });
    expect(gridLayout(1, 0)).toEqual({ columns: 1, span: 1, lastSpan: 1, lastFrom: 1 });
  });
});
