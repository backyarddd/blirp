import { describe, expect, it } from 'vitest';
import { formatElapsed, formatRelative } from './time';

describe('formatElapsed', () => {
  it('formats each magnitude', () => {
    expect(formatElapsed(-5)).toBe('0s');
    expect(formatElapsed(42_000)).toBe('42s');
    expect(formatElapsed(425_000)).toBe('7m 05s');
    expect(formatElapsed(2 * 3_600_000 + 3 * 60_000)).toBe('2h 03m');
    expect(formatElapsed(3 * 86_400_000 + 4 * 3_600_000)).toBe('3d 4h');
  });
});

describe('formatRelative', () => {
  const now = new Date(2026, 8, 25, 15, 0, 0).getTime();
  it('uses short relative forms', () => {
    expect(formatRelative(now - 10_000, now)).toBe('just now');
    expect(formatRelative(now - 5 * 60_000, now)).toBe('5m ago');
    expect(formatRelative(now - 3 * 3_600_000, now)).toBe('3h ago');
    expect(formatRelative(new Date(2026, 8, 24, 23, 0).getTime(), now)).toBe('Yesterday');
  });
  it('falls back to dates', () => {
    expect(formatRelative(new Date(2024, 2, 4, 12).getTime(), now)).toMatch(/2024/);
    expect(formatRelative(new Date(2026, 2, 4, 12).getTime(), now)).not.toMatch(/2026/);
  });
});
