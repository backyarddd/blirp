import { describe, expect, it } from 'vitest';
import { TERMINAL_DEFAULTS, parseTerminalSettings } from './settings.svelte';

describe('parseTerminalSettings', () => {
  it('falls back to the defaults for missing or broken storage', () => {
    expect(parseTerminalSettings(null)).toEqual(TERMINAL_DEFAULTS);
    expect(parseTerminalSettings('not json')).toEqual(TERMINAL_DEFAULTS);
    expect(parseTerminalSettings('[1]')).toEqual(TERMINAL_DEFAULTS);
  });
  it('keeps valid fields and clamps or drops the rest one by one', () => {
    expect(
      parseTerminalSettings(
        JSON.stringify({ fontSize: 99, lineHeight: 1.2, letterSpacing: 'x', cursorStyle: 'bar', cursorBlink: 'yes', macOptionIsMeta: false }),
      ),
    ).toEqual({ ...TERMINAL_DEFAULTS, fontSize: 40, lineHeight: 1.2, cursorStyle: 'bar', macOptionIsMeta: false });
    expect(parseTerminalSettings(JSON.stringify({ fontSize: 12.6, cursorStyle: 'beam' }))).toEqual({ ...TERMINAL_DEFAULTS, fontSize: 13 });
  });
});
