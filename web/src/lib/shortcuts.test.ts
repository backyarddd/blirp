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
    expect(matchShortcut(k('ArrowLeft', { ctrl: true }), false, false)).toBe('prev');
    expect(matchShortcut(k('PageDown', { ctrl: true }), false, false)).toBe('next');
  });
  it('leaves every key but the allowlist to a focused terminal off mac', () => {
    expect(matchShortcut(k('w', { ctrl: true }), false, true)).toBeNull();
    expect(matchShortcut(k('W', { ctrl: true, shift: true }), false, true)).toBe('close');
    expect(matchShortcut(k('K', { ctrl: true, shift: true }), false, true)).toBe('palette');
    expect(matchShortcut(k('PageUp', { ctrl: true }), false, true)).toBe('prev');
    expect(matchShortcut(k('PageDown', { ctrl: true }), false, true)).toBe('next');
    // Word jumps and word selection belong to the shell.
    for (const shift of [false, true]) {
      expect(matchShortcut(k('ArrowLeft', { ctrl: true, shift }), false, true)).toBeNull();
      expect(matchShortcut(k('ArrowRight', { ctrl: true, shift }), false, true)).toBeNull();
    }
    expect(matchShortcut(k('PageUp', { ctrl: true, shift: true }), false, true)).toBeNull();
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
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'windows', false)).toBe('paste');
    expect(terminalClipboardKey(k('V', { ctrl: true }), 'windows', false)).toBe('paste');
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'linux', false)).toBeNull();
    expect(terminalClipboardKey(k('v', { ctrl: true }), 'mac', false)).toBeNull();
  });
  it('uses Ctrl+Shift off mac and Cmd on mac', () => {
    for (const p of ['windows', 'linux'] as const) {
      expect(terminalClipboardKey(k('V', { ctrl: true, shift: true }), p, false)).toBe('paste');
      expect(terminalClipboardKey(k('C', { ctrl: true, shift: true }), p, true)).toBe('copy');
      expect(terminalClipboardKey(k('v', { meta: true }), p, false)).toBeNull();
    }
    expect(terminalClipboardKey(k('v', { meta: true }), 'mac', false)).toBe('paste');
    expect(terminalClipboardKey(k('c', { meta: true }), 'mac', true)).toBe('copy');
    expect(terminalClipboardKey(k('v', { ctrl: true, shift: true }), 'mac', false)).toBeNull();
  });
  it('copies only with a selection, and on Windows plain Ctrl+C copies one', () => {
    expect(terminalClipboardKey(k('C', { ctrl: true, shift: true }), 'linux', false)).toBeNull();
    expect(terminalClipboardKey(k('c', { meta: true }), 'mac', false)).toBeNull();
    expect(terminalClipboardKey(k('c', { ctrl: true }), 'windows', true)).toBe('copy-clear');
    expect(terminalClipboardKey(k('c', { ctrl: true }), 'windows', false)).toBeNull();
    expect(terminalClipboardKey(k('c', { ctrl: true }), 'linux', true)).toBeNull();
    expect(terminalClipboardKey(k('c', { ctrl: true }), 'mac', true)).toBeNull();
  });
  it('pastes on Shift+Insert on Linux', () => {
    expect(terminalClipboardKey(k('Insert', { shift: true }), 'linux', false)).toBe('paste');
    expect(terminalClipboardKey(k('Insert', { shift: true }), 'windows', false)).toBeNull();
  });
  it('leaves Alt chords and other keys to the program', () => {
    for (const p of ['windows', 'linux', 'mac'] as const) {
      expect(terminalClipboardKey(k('v', { ctrl: true, alt: true }), p, true)).toBeNull();
      expect(terminalClipboardKey(k('v'), p, true)).toBeNull();
      expect(terminalClipboardKey(k('x', { ctrl: true, shift: true }), p, true)).toBeNull();
    }
  });
});
