// Loading / error page of the desktop shell. The Rust side navigates away to
// the daemon's UI once it is healthy; until then this page polls the startup
// phase and offers Retry on failure.
'use strict';

const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

async function poll() {
  let state;
  try {
    state = await invoke('startup_state');
  } catch (err) {
    state = { phase: 'failed', message: String(err), log_dir: '' };
  }
  const failed = state.phase === 'failed';
  $('starting').hidden = failed;
  $('failed').hidden = !failed;
  if (failed) {
    $('message').textContent = state.message;
    $('logdir').textContent = state.log_dir;
    return;
  }
  setTimeout(poll, 400);
}

$('retry').addEventListener('click', async () => {
  $('failed').hidden = true;
  $('starting').hidden = false;
  try {
    await invoke('retry');
  } finally {
    setTimeout(poll, 400);
  }
});

$('logs').addEventListener('click', () => {
  invoke('open_logs').catch((err) => {
    $('message').textContent += `\n\nCould not open the log folder: ${err}`;
  });
});

poll();
