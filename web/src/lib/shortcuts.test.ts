import { describe, expect, it } from 'vitest';
import { matchShortcut } from './shortcuts';

const k = (key: string, mods: Partial<{ ctrl: boolean; meta: boolean; shift: boolean; alt: boolean }> = {}) => ({
  key,
  ctrlKey: mods.ctrl ?? false,
  metaKey: mods.meta ?? false,
  shiftKey: mods.shift ?? false,
  altKey: mods.alt ?? false,
});

describe('matchShortcut', () => {
  it('uses Ctrl off mac and Cmd on mac', () => {
    expect(matchShortcut(k('k', { ctrl: true }), false, false)).toBe('palette');
    expect(matchShortcut(k('k', { meta: true }), false, false)).toBeNull();
    expect(matchShortcut(k('k', { meta: true }), true, false)).toBe('palette');
    expect(matchShortcut(k('k', { ctrl: true }), true, false)).toBeNull();
  });
  it('leaves plain Ctrl chords to the terminal off mac', () => {
    expect(matchShortcut(k('w', { ctrl: true }), false, true)).toBeNull();
    expect(matchShortcut(k('W', { ctrl: true, shift: true }), false, true)).toBe('close');
    expect(matchShortcut(k('ArrowLeft', { meta: true }), true, true)).toBe('prev');
  });
  it('ignores alt and unknown keys', () => {
    expect(matchShortcut(k('g', { ctrl: true, alt: true }), false, false)).toBeNull();
    expect(matchShortcut(k('x', { ctrl: true }), false, false)).toBeNull();
    expect(matchShortcut(k('g', { ctrl: true }), false, false)).toBe('grid');
  });
});
