import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { bootstrapToken } from './lib/api/token';
import { app } from './lib/app.svelte';
import { takeDesktopVersion } from './lib/update';

/** The desktop shell's version lives in this window's sessionStorage. */
const fragmentEnv = (): Parameters<typeof takeDesktopVersion>[0] => ({
  location,
  history,
  storage: () => sessionStorage,
});

// Before the first request: take the sign-in token (and the desktop app's version) out of the
// URL fragment.
takeDesktopVersion(fragmentEnv());
bootstrapToken();
// The desktop app signs its window in again after a daemon restart (new token) by setting
// `#token=` on the page it shows, which does not reload it: start over with the new token. A
// reload would lose a token that only this page's memory holds (blocked storage); reconnect in place.
window.addEventListener('hashchange', () => {
  takeDesktopVersion(fragmentEnv());
  const taken = bootstrapToken();
  if (taken === 'stored') {
    location.reload();
  } else if (taken === 'memory') {
    app.stopStream();
    void app.boot();
  }
});

const target = document.getElementById('app');
if (!target) throw new Error('#app mount point missing from index.html');

export default mount(App, { target });
