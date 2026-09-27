// Session attention alerts. OS notifications go through the desktop app's `notify` command
// when the page runs inside it (its webview does not deliver the Web Notification API to the
// OS), else through the browser's Notification API. In-app signals (toast, title count,
// favicon dot, optional sound) work everywhere, with or without OS permission.

import type { SessionStatus } from './api/types.gen';
import { readPref, writePref } from './prefs';
import { notifiableTransition } from './status';

export type NotifyEvent = 'waiting' | 'completed' | 'failed';

export const NOTIFY_EVENTS: readonly { id: NotifyEvent; label: string }[] = [
  { id: 'waiting', label: 'Needs input' },
  { id: 'completed', label: 'Finished' },
  { id: 'failed', label: 'Failed' },
];

export interface NotifyPrefs {
  enabled: boolean;
  events: Record<NotifyEvent, boolean>;
  sound: boolean;
}

const ON_OFF = ['on', 'off'] as const;

export function readNotifyPrefs(): NotifyPrefs {
  const on = (key: string, fallback: 'on' | 'off'): boolean => readPref(key, ON_OFF, fallback) === 'on';
  return {
    enabled: on('blirp.notify', 'on'),
    events: {
      waiting: on('blirp.notify.waiting', 'on'),
      completed: on('blirp.notify.completed', 'on'),
      failed: on('blirp.notify.failed', 'on'),
    },
    sound: on('blirp.notify.sound', 'off'),
  };
}

export function writeNotifyPrefs(p: NotifyPrefs): void {
  const w = (key: string, v: boolean): void => writePref(key, v ? 'on' : 'off');
  w('blirp.notify', p.enabled);
  for (const e of NOTIFY_EVENTS) w(`blirp.notify.${e.id}`, p.events[e.id]);
  w('blirp.notify.sound', p.sound);
}

/**
 * How to tell the user about a status change: nothing, an in-app toast (the window has focus
 * but shows something else), or an OS notification (the window is hidden or unfocused).
 */
export type Delivery = 'toast' | 'system';

export interface NotifyContext {
  /** The window is visible and has focus. */
  focused: boolean;
  /** The changed session is on screen (its pane, or its tile in the grid). */
  viewing: boolean;
}

export function notifyDecision(
  prefs: NotifyPrefs,
  prev: SessionStatus,
  next: SessionStatus,
  ctx: NotifyContext,
): { event: NotifyEvent; delivery: Delivery } | null {
  if (!prefs.enabled || !notifiableTransition(prev, next)) return null;
  const event = next as NotifyEvent;
  if (!prefs.events[event]) return null;
  if (ctx.focused && ctx.viewing) return null;
  return { event, delivery: ctx.focused ? 'toast' : 'system' };
}

export const EVENT_TEXT: Record<NotifyEvent, string> = {
  waiting: 'needs your input',
  completed: 'finished',
  failed: 'failed',
};

// ---------------------------------------------------------------- OS notifications

declare global {
  interface Window {
    /** Injected by the desktop app (Tauri) into its webview. */
    __TAURI_INTERNALS__?: { invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown> };
  }
}

export type Backend = 'desktop' | 'browser' | 'unsupported';

export function backend(): Backend {
  if (typeof window.__TAURI_INTERNALS__?.invoke === 'function') return 'desktop';
  return 'Notification' in window ? 'browser' : 'unsupported';
}

/** Browser permission, or what stands in for it. */
export type Permission = NotificationPermission | 'unsupported' | 'desktop';

export function permission(): Permission {
  const b = backend();
  if (b === 'desktop') return 'desktop';
  return b === 'browser' ? Notification.permission : 'unsupported';
}

export type SystemResult = { ok: true; detail: string } | { ok: false; detail: string };

interface DesktopReport {
  backend: string;
  problem: string | null;
}

function isDesktopReport(v: unknown): v is DesktopReport {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as { backend?: unknown; problem?: unknown };
  return typeof o.backend === 'string' && (o.problem === null || typeof o.problem === 'string');
}

/** Show an OS notification. `onClick` runs when a browser notification is clicked. */
export async function showSystem(title: string, body: string, tag: string, onClick: () => void): Promise<SystemResult> {
  const invoke = window.__TAURI_INTERNALS__?.invoke;
  let desktopError = '';
  if (typeof invoke === 'function') {
    try {
      const r: unknown = await invoke('notify', { title, body });
      if (!isDesktopReport(r)) return { ok: false, detail: 'The desktop app sent an unexpected reply.' };
      return r.problem ? { ok: false, detail: r.problem } : { ok: true, detail: `Sent to ${r.backend}.` };
    } catch (e) {
      // An older desktop app without the command: the webview's own API may still work.
      desktopError = `The desktop app refused the notification (${String(e)}). `;
    }
  }
  if (!('Notification' in window)) return { ok: false, detail: `${desktopError}This browser has no notification support.` };
  if (Notification.permission !== 'granted') {
    return { ok: false, detail: desktopError + permissionHint(Notification.permission) };
  }
  try {
    const n = new Notification(title, { body, tag });
    n.onclick = () => {
      window.focus();
      onClick();
      n.close();
    };
    return { ok: true, detail: `${desktopError}Sent to the browser.` };
  } catch (e) {
    return { ok: false, detail: `${desktopError}The browser refused: ${String(e)}` };
  }
}

export function permissionHint(p: Permission): string {
  switch (p) {
    case 'granted':
    case 'desktop':
      return '';
    case 'default':
      return 'This browser has not been allowed to show notifications yet: use "Enable desktop notifications".';
    case 'denied':
      return 'Notifications are blocked for this site. Allow them in the site settings (the icon left of the address bar), then reload.';
    case 'unsupported':
      return window.isSecureContext
        ? 'This browser has no notification support.'
        : 'Browsers only allow notifications on https:// or localhost pages.';
  }
}

// ---------------------------------------------------------------- sound

let audio: AudioContext | null = null;

/** Create the audio context from a user gesture so later chimes are allowed to play. */
export function primeSound(): void {
  try {
    audio ??= new AudioContext();
    void audio.resume();
  } catch (e) {
    console.warn('blirp: no audio for notification sounds', e);
  }
}

export function playChime(): void {
  primeSound();
  if (!audio) return;
  const t = audio.currentTime;
  const osc = audio.createOscillator();
  const gain = audio.createGain();
  osc.frequency.setValueAtTime(880, t);
  osc.frequency.setValueAtTime(1320, t + 0.09);
  gain.gain.setValueAtTime(0.0001, t);
  gain.gain.exponentialRampToValueAtTime(0.15, t + 0.02);
  gain.gain.exponentialRampToValueAtTime(0.0001, t + 0.3);
  osc.connect(gain).connect(audio.destination);
  osc.start(t);
  osc.stop(t + 0.32);
}

// ---------------------------------------------------------------- title and favicon badge

let baseTitle: string | null = null;
const iconHrefs = new Map<HTMLLinkElement, string>();
let dotted: string | null = null;

/** "(n) blirp" and a dot on the favicon while n sessions want attention; restores both at 0. */
export function setBadge(n: number): void {
  baseTitle ??= document.title;
  document.title = n > 0 ? `(${n}) ${baseTitle}` : baseTitle;
  const links = [...document.querySelectorAll<HTMLLinkElement>('link[rel="icon"]')];
  for (const l of links) if (!iconHrefs.has(l)) iconHrefs.set(l, l.href);
  if (n === 0) {
    for (const [l, href] of iconHrefs) l.href = href;
    return;
  }
  if (dotted) {
    for (const l of links) l.href = dotted;
    return;
  }
  const png = links.find((l) => l.type === 'image/png') ?? links[0];
  if (!png) return;
  const img = new Image();
  img.onload = () => {
    const c = document.createElement('canvas');
    c.width = c.height = 64;
    const g = c.getContext('2d');
    if (!g) return;
    g.drawImage(img, 0, 0, 64, 64);
    g.fillStyle = '#e5484d';
    g.strokeStyle = '#ffffff';
    g.lineWidth = 4;
    g.beginPath();
    g.arc(48, 16, 14, 0, Math.PI * 2);
    g.fill();
    g.stroke();
    dotted = c.toDataURL('image/png');
    // Still wanted: the count may have dropped to 0 while the image loaded.
    if (document.title !== baseTitle) for (const l of links) l.href = dotted;
  };
  img.src = iconHrefs.get(png) ?? png.href;
}
