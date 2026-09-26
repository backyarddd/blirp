import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { bootstrapToken } from './lib/api/token';

// Before the first request: take the sign-in token out of the URL fragment.
bootstrapToken();

const target = document.getElementById('app');
if (!target) throw new Error('#app mount point missing from index.html');

export default mount(App, { target });
