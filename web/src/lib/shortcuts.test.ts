import { describe, expect, it } from 'vitest';
import { matchShortcut, terminalClipboardKey } from './shortcuts';

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

describe('terminalClipboardKey', () => {
  it('pastes on plain Ctrl+V only on Windows', () => {
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'windows')).toBe('paste');
    expect(terminalClipboardKey(k('V', { ctrl: true }), 'windows')).toBe('paste');
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'linux')).toBeNull();
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'mac')).toBeNull();
  });
  it('uses Ctrl+Shift off mac and Cmd on mac', () => {
    for (const p of ['windows', 'linux'] as const) {
      expect(terminalClipboardKey(k('V', { ctrl: true, shift: true }), p)).toBe('paste');
      expect(terminalClipboardKey(k('C', { ctrl: true, shift: true }), p)).toBe('copy');
      expect(terminalClipboardKey(k('v', { meta: true }), p)).toBeNull();
    }
    expect(terminalClipboardKey(k('v', { meta: true }), 'mac')).toBe('paste');
    expect(terminalClipboardKey(k('c', { meta: true }), 'mac')).toBe('copy');
    expect(terminalClipboardKey(k('v', { ctrl: true, shift: true }), 'mac')).toBeNull();
  });
  it('leaves Ctrl+C, Alt chords and other keys to the program', () => {
    for (const p of ['windows', 'linux', 'mac'] as const) {
      expect(terminalClipboardKey(k('c', { ctrl: true }), p)).toBeNull();
      expect(terminalClipboardKey(k('v', { ctrl: true, alt: true }), p)).toBeNull();
      expect(terminalClipboardKey(k('v'), p)).toBeNull();
      expect(terminalClipboardKey(k('x', { ctrl: true, shift: true }), p)).toBeNull();
    }
  });
});
