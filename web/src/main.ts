import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { bootstrapToken } from './lib/api/token';

// Before the first request: take the sign-in token out of the URL fragment.
bootstrapToken();
// The desktop app signs its window in again after a daemon restart (new token) by setting
// `#token=` on the page it shows, which does not reload it: start over with the new token.
window.addEventListener('hashchange', () => {
  if (bootstrapToken()) location.reload();
});

const target = document.getElementById('app');
if (!target) throw new Error('#app mount point missing from index.html');

export default mount(App, { target });
