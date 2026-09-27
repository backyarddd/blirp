import { afterEach, describe, expect, it, vi } from 'vitest';
import type { SessionStatus } from './api/types.gen';
import { notifyDecision, readNotifyPrefs, type NotifyPrefs } from './notify';

const ALL: SessionStatus[] = ['starting', 'working', 'idle', 'waiting', 'completed', 'failed', 'detached'];
const PREFS: NotifyPrefs = { enabled: true, events: { waiting: true, completed: true, failed: true }, sound: false };
const AWAY = { focused: false, viewing: false };

describe('notifyDecision', () => {
  it('fires only on a change into waiting, completed or failed', () => {
    for (const prev of ALL) {
      for (const next of ALL) {
        const d = notifyDecision(PREFS, prev, next, AWAY);
        const expected = prev !== next && (next === 'waiting' || next === 'completed' || next === 'failed');
        expect(d !== null, `${prev} -> ${next}`).toBe(expected);
      }
    }
    expect(notifyDecision(PREFS, 'working', 'waiting', AWAY)).toEqual({ event: 'waiting', delivery: 'system' });
  });

  it('stays quiet for the session the user is looking at in a focused window', () => {
    expect(notifyDecision(PREFS, 'working', 'waiting', { focused: true, viewing: true })).toBeNull();
  });

  it('uses an in-app toast when the window has focus but shows another session', () => {
    expect(notifyDecision(PREFS, 'working', 'completed', { focused: true, viewing: false })).toEqual({
      event: 'completed',
      delivery: 'toast',
    });
  });

  it('uses the OS when the window is hidden or unfocused, even on that session', () => {
    expect(notifyDecision(PREFS, 'working', 'failed', { focused: false, viewing: true })?.delivery).toBe('system');
  });

  it('respects the master switch and per-event choices', () => {
    expect(notifyDecision({ ...PREFS, enabled: false }, 'working', 'waiting', AWAY)).toBeNull();
    const noFinish = { ...PREFS, events: { ...PREFS.events, completed: false } };
    expect(notifyDecision(noFinish, 'working', 'completed', AWAY)).toBeNull();
    expect(notifyDecision(noFinish, 'working', 'waiting', AWAY)).not.toBeNull();
  });
});

describe('readNotifyPrefs', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('defaults to on for every event with sound off when nothing is stored', () => {
    expect(readNotifyPrefs()).toEqual(PREFS);
  });

  it('keeps an earlier explicit "off" and reads the other choices', () => {
    const stored: Record<string, string> = { 'blirp.notify': 'off', 'blirp.notify.failed': 'off', 'blirp.notify.sound': 'on' };
    vi.stubGlobal('localStorage', { getItem: (k: string) => stored[k] ?? null });
    expect(readNotifyPrefs()).toEqual({ enabled: false, events: { waiting: true, completed: true, failed: false }, sound: true });
  });
});
