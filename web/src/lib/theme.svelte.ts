import { readPref, writePref } from './prefs';

export type ThemePref = 'system' | 'light' | 'dark';
const KEY = 'blirp.theme';

const media = window.matchMedia('(prefers-color-scheme: dark)');

class Theme {
  pref: ThemePref = $state(readPref<ThemePref>(KEY, ['system', 'light', 'dark'], 'system'));
  systemDark: boolean = $state(media.matches);
  resolved: 'light' | 'dark' = $derived(this.pref === 'system' ? (this.systemDark ? 'dark' : 'light') : this.pref);
}

export const theme = new Theme();

media.addEventListener('change', (e) => {
  theme.systemDark = e.matches;
});

export function setTheme(pref: ThemePref): void {
  theme.pref = pref;
  writePref(KEY, pref);
}

export function toggleTheme(): void {
  setTheme(theme.resolved === 'dark' ? 'light' : 'dark');
}

/** Keeps `<html data-theme>` in sync; call once during root component init. */
export function applyTheme(): void {
  $effect(() => {
    document.documentElement.dataset.theme = theme.resolved;
  });
}
