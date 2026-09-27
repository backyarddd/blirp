// Drives the built SPA against the real daemon: auth, projects (git, plain folder, no folder), chats,
// a live shell session, memory, wiki, resources, files, git, search, settings, palette,
// mobile layout, and no CSP violations along the way. Tests share one page and run in order.
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { e2eEnv } from './env';

const env = e2eEnv();
const isWindows = process.platform === 'win32';
const STOPPED = 'Stopped';
/** The daemon's loopback API takes the runtime token as a bearer token only (no cookies). */
const AUTH = { Authorization: `Bearer ${env.token}` };

test.describe.configure({ mode: 'serial' });

let page: Page;
const consoleErrors: string[] = [];
let plainProjectUrl = '';
let repoProjectUrl = '';
let sessionId = '';

interface Snapshot {
  cols: number;
  rows: number;
  data: string;
}

/**
 * Attach a second client to the session's terminal and return the daemon's screen snapshot. Like
 * the UI, it trades the stored token for a single-use WebSocket ticket first.
 */
function snapshot(id: string): Promise<Snapshot> {
  return page.evaluate(
    async (sid) => {
      const path = `/api/terminals/${sid}/ws`;
      const res = await fetch('/api/ws-ticket', {
        method: 'POST',
        headers: { Authorization: `Bearer ${localStorage.getItem('blirp.token') ?? ''}`, 'Content-Type': 'application/json' },
        body: JSON.stringify({ path }),
      });
      if (!res.ok) throw new Error(`ws-ticket: ${res.status}`);
      const { ticket } = (await res.json()) as { ticket: string };
      return new Promise<Snapshot>((resolve, reject) => {
        const ws = new WebSocket(`ws://${location.host}${path}?ticket=${ticket}`);
        const timer = setTimeout(() => reject(new Error('no snapshot within 10 s')), 10_000);
        ws.onmessage = (ev) => {
          if (typeof ev.data !== 'string') return;
          const m = JSON.parse(ev.data) as { type: string } & Snapshot;
          if (m.type !== 'snapshot') return;
          clearTimeout(timer);
          ws.close();
          resolve({ cols: m.cols, rows: m.rows, data: m.data });
        };
        ws.onerror = () => reject(new Error('terminal websocket failed'));
      });
    },
    id,
  );
}

/**
 * Type a command into the focused pane until its output shows up in the PTY screen. A shell
 * can report idle before its line editor is ready, so a first attempt may be swallowed.
 */
async function runInTerminal(id: string, command: string, marker: string): Promise<void> {
  await expect(async () => {
    await page.locator('.xterm').click();
    await page.keyboard.type(command);
    await page.keyboard.press('Enter');
    await expect.poll(async () => (await snapshot(id)).data, { timeout: 5_000 }).toContain(marker);
  }).toPass({ timeout: 30_000 });
}

async function addFolder(path: string): Promise<string> {
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  await page.getByRole('button', { name: 'New project' }).click();
  const dialog = page.getByRole('dialog', { name: 'New project' });
  await dialog.getByLabel(/^Folder/).fill(path);
  await dialog.getByRole('button', { name: 'Create project' }).click();
  await expect(page).toHaveURL(/\/projects\/[^/]+$/);
  return new URL(page.url()).pathname;
}

async function confirmNextDialog(): Promise<void> {
  page.once('dialog', (d) => void d.accept());
}

test.beforeAll(async ({ browser }) => {
  const context = await browser.newContext();
  await context.addInitScript(() => {
    const w = window as unknown as { __csp: string[] };
    w.__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => w.__csp.push(`${e.violatedDirective} ${e.blockedURI}`));
  });
  page = await context.newPage();
  page.on('console', (m) => {
    if (m.type() === 'error') consoleErrors.push(m.text());
  });
  page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));
});

test.afterAll(async () => {
  await page?.context().close();
});

test('rejects a missing login, then signs in with /#token= and keeps no cookie', async () => {
  await page.goto(`${env.url}/`);
  await expect(page.getByRole('heading', { name: 'Sign in required' })).toBeVisible();
  // The desktop app signs its window in again after a daemon restart by setting `#token=` on the
  // page it shows, which does not reload it.
  await page.evaluate((t) => {
    location.hash = `token=${t}`;
  }, env.token);
  await expect(page.getByRole('heading', { name: 'Pick a session' })).toBeVisible();
  await expect(page).toHaveURL(`${env.url}/`);
  await page.evaluate(() => localStorage.clear());
  await page.goto(`${env.url}/sessions#token=${env.token}`);
  await expect(page).toHaveURL(`${env.url}/sessions`);
  await expect(page.getByRole('heading', { name: 'Pick a session' })).toBeVisible();
  expect(await page.evaluate(() => localStorage.getItem('blirp.token'))).toBe(env.token);
  expect(await page.context().cookies()).toEqual([]);
  consoleErrors.length = 0; // the 401 before signing in is expected
  const csp = (await page.request.get(`${env.url}/`)).headers()['content-security-policy'] ?? '';
  expect(csp).toContain("script-src 'self';");
  expect(csp).toContain("style-src 'self' 'unsafe-inline'");
  expect(csp).toContain("img-src 'self' data:");
  expect(csp).toContain(`connect-src 'self' ws://${new URL(env.url).host}`);
});

test('registers a plain folder as a first-class project', async () => {
  plainProjectUrl = await addFolder(env.plain);
  await expect(page.getByRole('heading', { level: 1, name: 'Plain Folder' })).toBeVisible();
  await expect(page.locator('.head .badge')).toHaveText('folder');
  const tabs = page.getByRole('navigation', { name: 'Project sections' });
  await expect(tabs.getByRole('link', { name: 'Files' })).toBeVisible();
  await expect(tabs.getByRole('link', { name: 'Git' })).toHaveCount(0);
  await expect(page.getByRole('checkbox', { name: 'New worktree' })).toHaveCount(0);
});

test('registers a git repository', async () => {
  repoProjectUrl = await addFolder(env.repo);
  await expect(page.getByRole('heading', { level: 1, name: 'git-repo' })).toBeVisible();
  await expect(page.locator('.head .badge')).toHaveText('git');
  await expect(page.getByRole('navigation', { name: 'Project sections' }).getByRole('link', { name: 'Git' })).toBeVisible();
  await expect(page.getByRole('checkbox', { name: 'New worktree' })).toBeVisible();
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  await expect(page.locator('.pcard')).toHaveCount(2);
});

test('launches a shell session in the plain folder', async () => {
  await page.getByRole('button', { name: 'New session' }).first().click();
  const dialog = page.getByRole('dialog', { name: 'New session' });
  const project = dialog.getByLabel('Project');
  await project.selectOption({ label: 'git-repo' });
  await expect(dialog.getByRole('checkbox', { name: /new git worktree/ })).toBeVisible();
  await project.selectOption({ label: 'Plain Folder (folder)' });
  await expect(dialog.getByRole('checkbox', { name: /new git worktree/ })).toHaveCount(0);
  await dialog.getByLabel('Agent').selectOption('shell');
  await page.waitForTimeout(500); // let the dialog's async defaults land; they must not override the choice
  await expect(dialog.getByLabel('Agent')).toHaveValue('shell');
  await dialog.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(/\/sessions\/[^/]+$/);
  sessionId = new URL(page.url()).pathname.split('/').pop() ?? '';

  const card = page.getByRole('complementary', { name: 'Sessions' }).locator(`a[href="/sessions/${sessionId}"]`);
  await expect(card).toContainText('Shell in Plain Folder');
  await expect(card).toContainText('Plain Folder'); // folder, not a branch
  await expect(card.locator('.chip')).toHaveText(/Starting|Working|Idle/);
  // The shell prints its prompt, then goes quiet: the status heuristic reports idle.
  await expect(card.locator('.chip')).toHaveText('Idle', { timeout: 30_000 });
});

test('types into the terminal and sees the output', async () => {
  await runInTerminal(sessionId, isWindows ? "Write-Output ('e2e-' + (6*7))" : 'echo e2e-$((6*7))', 'e2e-42');
});

/** Files saved for pasted or dropped uploads of a session (`BLIRP_HOME/uploads/<id>/`). */
function uploads(id: string): string[] {
  const dir = join(env.root, 'home', 'uploads', id);
  return existsSync(dir) ? readdirSync(dir) : [];
}

/**
 * Put a file into the focused pane the way the browser would: a synthetic paste (a screenshot on
 * the clipboard) or drop (a file dragged from the desktop). Resolves once the upload answered.
 */
async function transferFile(kind: 'paste' | 'drop', name: string, type: string): Promise<void> {
  const uploaded = page.waitForResponse((r) => r.url().includes(`/api/sessions/${sessionId}/uploads`));
  await page.evaluate(
    ({ kind, name, type }) => {
      const dt = new DataTransfer();
      dt.items.add(new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47, 1, 2, 3])], name, { type }));
      if (kind === 'paste') {
        const target = document.querySelector('.xterm-helper-textarea');
        if (!target) throw new Error('no xterm textarea');
        target.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      } else {
        const target = document.querySelector('.xterm-screen');
        if (!target) throw new Error('no xterm screen');
        target.dispatchEvent(new DragEvent('dragover', { dataTransfer: dt, bubbles: true, cancelable: true }));
        target.dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
      }
    },
    { kind, name, type },
  );
  expect((await uploaded).status()).toBe(201);
  // The pane pastes the saved path right after reading the answer.
  await page.waitForTimeout(300);
}

test('a pasted image and a dropped file reach the PTY as paths to the saved files', async () => {
  for (const [kind, name, type, word] of [
    ['paste', 'e2e-shot.png', 'image/png', 'pasted'],
    ['drop', 'e2e notes.txt', 'text/plain', 'dropped'],
  ] as const) {
    await expect(async () => {
      await page.locator('.xterm').click();
      await page.keyboard.press('Control+C'); // an empty line for this attempt
      // The shell only prints the marker when the pasted text is the path of an existing file.
      await page.keyboard.type(isWindows ? 'if (Test-Path ' : 'test -f ');
      await transferFile(kind, name, type);
      await page.keyboard.type(isWindows ? `) { '${word}-' + (6*7) }` : `&& echo ${word}-$((6*7))`);
      await page.keyboard.press('Enter');
      await expect.poll(async () => (await snapshot(sessionId)).data, { timeout: 5_000 }).toContain(`${word}-42`);
    }).toPass({ timeout: 45_000 });
  }
  const saved = uploads(sessionId);
  expect(saved.some((f) => /^[0-9]+-e2e-shot[.]png$/.test(f))).toBe(true);
  // Sanitized: the space in the dropped name never reaches the file system.
  expect(saved.some((f) => /^[0-9]+-e2e_notes[.]txt$/.test(f))).toBe(true);
  const shot = saved.find((f) => f.endsWith('e2e-shot.png')) ?? '';
  expect([...readFileSync(join(env.root, 'home', 'uploads', sessionId, shot))]).toEqual([0x89, 0x50, 0x4e, 0x47, 1, 2, 3]);
});

/** The daemon's own log (`BLIRP_HOME/logs/blirpd.<date>.log`), all days. */
function daemonLog(): string {
  const dir = join(env.root, 'home', 'logs');
  if (!existsSync(dir)) return '';
  return readdirSync(dir)
    .filter((f) => f.startsWith('blirpd'))
    .map((f) => readFileSync(join(dir, f), 'utf8'))
    .join('\n');
}

test('reopening the app restores the open session, reattached, and shows keep-awake', async () => {
  await page.goto(`${env.url}/`);
  await expect(page).toHaveURL(`${env.url}/sessions/${sessionId}`);
  // macOS and Windows always grant the assertion. On Linux logind may refuse
  // systemd-inhibit (polkit, outside a login session as on CI runners): then
  // the daemon must say why and the top bar must not claim it. A slow refusal
  // lands after the helper's settle window and is logged as an ended assertion.
  const refused = /cannot keep this machine awake|sleep prevention ended unexpectedly.* error=/;
  if (process.platform === 'linux' && !(await apiCall<{ keep_awake: boolean }>('GET', '/api/health')).keep_awake) {
    await expect.poll(daemonLog, { timeout: 15_000 }).toMatch(refused);
    await expect(page.getByTestId('keep-awake')).toHaveCount(0);
  } else {
    await expect(page.getByTestId('keep-awake')).toBeVisible();
  }
  // The snapshot of the reattached pane still holds the earlier output.
  expect((await snapshot(sessionId)).data).toContain('e2e-42');
  await runInTerminal(sessionId, isWindows ? "Write-Output ('back-' + (6*7))" : 'echo back-$((6*7))', 'back-42');
});

test('resizing the pane resizes the PTY', async () => {
  const before = await snapshot(sessionId);
  // Narrow enough that the sidebar becomes a drawer and the terminal loses width and height.
  await page.setViewportSize({ width: 640, height: 600 });
  await expect.poll(async () => (await snapshot(sessionId)).cols).toBeLessThan(before.cols);
  const after = await snapshot(sessionId);
  expect(after.rows).toBeLessThan(before.rows);
  // The pane's xterm matches the PTY size the daemon now reports.
  const rows = await page.locator('.xterm-rows > div, .xterm-screen canvas').count();
  expect(rows).toBeGreaterThan(0);
  await page.setViewportSize({ width: 1360, height: 860 });
});

test('stops the session and shows the exit state in the pane', async () => {
  await confirmNextDialog();
  await page.getByRole('button', { name: 'Stop' }).click();
  const exit = page.getByTestId('terminal-exit');
  await expect(exit).toContainText('Process exited');
  await expect(exit).toContainText(STOPPED);
  const card = page.getByRole('complementary', { name: 'Sessions' }).locator(`a[href="/sessions/${sessionId}"]`);
  await expect(card.locator('.chip')).toHaveText(STOPPED);
  await expect(page.getByRole('button', { name: 'Resume' })).toBeVisible();
  // `open` validates its target (the happy path would pop a window on this desktop).
  const bad = await page.request.post(`${env.url}/api/sessions/${sessionId}/open`, {
    data: { target: 'browser' },
    headers: { Origin: env.url, ...AUTH },
  });
  expect(bad.status()).toBe(400);
  expect(((await bad.json()) as { error: { code: string } }).error.code).toBe('invalid_request');

  // Resume relaunches the shell in the same row; the pane reattaches to the new process.
  await page.getByRole('button', { name: 'Resume' }).click();
  await expect(exit).toBeHidden();
  await expect(card.locator('.chip')).toHaveText('Idle', { timeout: 30_000 });
  await runInTerminal(sessionId, isWindows ? "Write-Output ('again-' + (6*7))" : 'echo again-$((6*7))', 'again-42');
  await confirmNextDialog();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(exit).toContainText(STOPPED);

  await exit.getByRole('button', { name: 'Show details' }).click();
  await expect(page.getByRole('heading', { name: 'Transcript' })).toBeVisible();
  await expect(page.locator('.detail .chip')).toHaveText(STOPPED);
  // A reload lands on the detail view directly, since the terminal is gone.
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Transcript' })).toBeVisible();
  // No live session left: the machine may sleep again (the daemon lets go within a tick).
  await expect.poll(async () => (await apiCall<{ keep_awake: boolean }>('GET', '/api/health')).keep_awake).toBe(false);
  await page.reload();
  await expect(page.getByTestId('keep-awake')).toHaveCount(0);
});

test('records CRUD', async () => {
  await page.goto(`${env.url}${plainProjectUrl}/memory`);
  const records = page.locator('section', { has: page.getByRole('heading', { name: 'Records' }) });
  await records.getByRole('button', { name: 'New record' }).click();
  await records.getByLabel('Kind', { exact: true }).selectOption('decision');
  await records.getByLabel('Title', { exact: true }).fill('Use frobnicator for caching');
  await records.getByLabel('Details', { exact: true }).fill('Chosen after benchmarking.');
  await records.getByRole('button', { name: 'Create' }).click();
  const rec = records.locator('article.rec', { hasText: 'Use frobnicator for caching' });
  await expect(rec).toBeVisible();

  await rec.getByRole('button', { name: 'Edit' }).click();
  const form = records.locator('article.rec form'); // the article's text is now in inputs
  await form.getByLabel('Title', { exact: true }).fill('Use frobnicator v2 for caching');
  await form.getByRole('button', { name: 'Save' }).click();
  const edited = records.locator('article.rec', { hasText: 'Use frobnicator v2 for caching' });
  await expect(edited).toBeVisible();

  await edited.getByRole('button', { name: 'Pin' }).click();
  await expect(edited.getByRole('button', { name: 'Unpin' })).toBeVisible();

  // A second record that stays around for the search test.
  await records.getByRole('button', { name: 'New record' }).click();
  await records.getByLabel('Title', { exact: true }).fill('Gotcha: quux needs warming');
  await records.getByRole('button', { name: 'Create' }).click();
  await expect(records.locator('article.rec', { hasText: 'quux needs warming' })).toBeVisible();

  await edited.getByRole('button', { name: 'Mark resolved' }).click();
  await expect(edited).toHaveCount(0); // filter shows active records
  await records.getByLabel('Record status').selectOption('all');
  const resolved = records.locator('article.rec', { hasText: 'Use frobnicator v2 for caching' });
  await expect(resolved.locator('.badge', { hasText: 'resolved' })).toBeVisible();
  await confirmNextDialog();
  await resolved.getByRole('button', { name: 'Delete' }).click();
  await expect(resolved).toHaveCount(0);
  await page.reload();
  await expect(page.locator('article.rec', { hasText: 'frobnicator' })).toHaveCount(0);
  await expect(page.locator('article.rec', { hasText: 'quux needs warming' })).toBeVisible();
});

test('brief edit and history revert', async () => {
  const brief = page.locator('section', { has: page.getByRole('heading', { name: 'Brief' }) });
  for (const text of ['Brief version one', 'Brief version two']) {
    await brief.getByRole('button', { name: 'Edit' }).click();
    await brief.getByLabel('Brief markdown').fill(text);
    await brief.getByRole('button', { name: 'Save' }).click();
    await expect(brief.locator('.md')).toHaveText(text);
  }
  await brief.getByRole('button', { name: 'History' }).click();
  const v1 = brief.locator('li.ver', { hasText: 'v1' });
  await confirmNextDialog();
  await v1.getByRole('button', { name: 'Revert' }).click();
  await expect(brief.locator('.brief .md').first()).toHaveText('Brief version one');
  await expect(brief.locator('li.ver', { hasText: 'v3' }).locator('.badge')).toHaveText('current');
});

test('wiki page create and edit', async () => {
  await page.getByRole('navigation', { name: 'Project sections' }).getByRole('link', { name: 'Wiki' }).click();
  await page.getByRole('button', { name: 'New page' }).first().click();
  await page.getByLabel('Title', { exact: true }).fill('Setup Notes');
  await page.getByLabel('Page markdown').fill('Run `make` first.');
  await page.getByRole('button', { name: 'Save page' }).click();
  await expect(page).toHaveURL(/\/wiki\/setup-notes$/);
  await expect(page.getByRole('heading', { level: 2, name: 'Setup Notes' })).toBeVisible();
  await page.getByRole('button', { name: 'Edit' }).click();
  await page.getByLabel('Page markdown').fill('Run `make all` first.');
  await page.getByRole('button', { name: 'Save page' }).click();
  await expect(page.locator('.content .md')).toContainText('make all');
  await page.reload();
  await expect(page.getByRole('link', { name: 'Setup Notes' })).toBeVisible();
});

test('resources add', async () => {
  await page.getByRole('navigation', { name: 'Project sections' }).getByRole('link', { name: 'Resources' }).click();
  await page.getByRole('button', { name: 'Add resource' }).click();
  await page.getByLabel('Kind', { exact: true }).selectOption('doc');
  await page.getByLabel('URL or path').fill('https://example.com/design');
  await page.getByLabel('Title', { exact: true }).fill('Design doc');
  await page.getByRole('button', { name: 'Save' }).click();
  const link = page.getByRole('link', { name: 'Design doc' });
  await expect(link).toHaveAttribute('href', 'https://example.com/design');
  await page.reload();
  await expect(page.getByRole('link', { name: 'Design doc' })).toBeVisible();
});

test('files tab browses a plain folder', async () => {
  await page.getByRole('navigation', { name: 'Project sections' }).getByRole('link', { name: 'Files' }).click();
  const tree = page.getByRole('navigation', { name: 'Project files' });
  await tree.getByRole('button', { name: 'sub' }).click();
  await expect(tree.getByRole('button', { name: 'inner.txt' })).toBeVisible();
  await tree.getByRole('button', { name: 'notes.txt' }).click();
  await expect(page.getByRole('region', { name: 'File viewer' })).toContainText('hello from a plain folder');
});

test('git tab shows branch, changes and a diff', async () => {
  await page.goto(`${env.url}${repoProjectUrl}/git`);
  await expect(page.locator('.badge', { hasText: 'main' })).toBeVisible();
  const changes = page.getByRole('navigation', { name: 'Changed files' });
  await expect(changes.getByRole('button', { name: /README\.md/ })).toBeVisible();
  await expect(changes.getByRole('button', { name: /new-file\.txt/ })).toContainText('??');
  await changes.getByRole('button', { name: /README\.md/ }).click();
  const diff = page.getByRole('region', { name: 'Diff' });
  await expect(diff.locator('.ln.add', { hasText: 'changed line' })).toBeVisible();
  await expect(diff.locator('.ln.del', { hasText: 'original line' })).toBeVisible();
});

test('search finds memory records', async () => {
  await page.getByRole('link', { name: 'Search' }).click();
  await page.getByLabel('Search query').fill('quux');
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  const hit = page.locator('a.hit').first();
  await expect(hit).toContainText('quux needs warming');
  await expect(hit.locator('mark')).toHaveText(/quux/i);
  await expect(hit).toContainText('Plain Folder');
  await hit.click();
  await expect(page).toHaveURL(new RegExp(`${plainProjectUrl}/memory$`));
});

test('settings load and save (only the changed values)', async () => {
  await page.goto(`${env.url}/settings/memory`);
  const idle = page.getByLabel('Distill after idle (seconds)');
  await expect(idle).toHaveValue('300');
  await idle.fill('600');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Memory settings saved')).toBeVisible();
  await page.reload();
  await expect(page.getByLabel('Distill after idle (seconds)')).toHaveValue('600');

  await page.goto(`${env.url}/settings/agents`);
  await expect(page.getByRole('heading', { name: 'Detected agents' })).toBeVisible();
  await expect(page.locator('li.agent[data-agent="shell"]')).toContainText('Shell');
  const worktree = page.getByRole('checkbox', { name: /new worktree by default/ });
  await worktree.check();
  await expect(page.getByText('Saved', { exact: true })).toBeVisible();
  await page.reload();
  await expect(page.getByRole('checkbox', { name: /new worktree by default/ })).toBeChecked();
  // The earlier memory change survived the second save.
  const res = await page.request.get(`${env.url}/api/settings`, { headers: AUTH });
  const view = (await res.json()) as { config: { memory: { distill_idle_secs: number }; sessions: { worktree_default: boolean } } };
  expect(view.config.memory.distill_idle_secs).toBe(600);
  expect(view.config.sessions.worktree_default).toBe(true);
});

/** Authenticated API call with the runtime token; fails the test on a non-2xx answer. */
async function apiCall<T>(method: 'GET' | 'POST' | 'DELETE', path: string, data?: unknown): Promise<T> {
  const res = await page.request.fetch(`${env.url}${path}`, { method, data, headers: { Origin: env.url, ...AUTH } });
  const text = await res.text();
  expect(res.ok(), `${method} ${path}: ${res.status()} ${text}`).toBe(true);
  return (text ? JSON.parse(text) : undefined) as T;
}

interface SessionRow {
  id: string;
  status: string;
  agent_session_id: string | null;
  parent_session_id: string | null;
  worktree: string | null;
}

const LIVE = ['starting', 'working', 'idle', 'waiting'];

/** Launches a shell session through the API, then stops it and waits until it has ended. */
async function endedShell(body: Record<string, unknown>): Promise<SessionRow> {
  const s = await apiCall<SessionRow>('POST', '/api/sessions', { agent: 'shell', ...body });
  await expect
    .poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status, { timeout: 30_000 })
    .toBe('idle');
  await apiCall('POST', `/api/sessions/${s.id}/stop`);
  await expect
    .poll(async () => LIVE.includes((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status), { timeout: 30_000 })
    .toBe(false);
  return apiCall<SessionRow>('GET', `/api/sessions/${s.id}`);
}

const idOf = (projectUrl: string): string => projectUrl.split('/').pop() ?? '';

test('memory panel shows the memory a shell session was given', async () => {
  const projectId = plainProjectUrl.split('/').pop() ?? '';
  await apiCall('POST', `/api/projects/${projectId}/records`, {
    kind: 'open_thread',
    title: 'E2E seeded thread',
    body: 'Seeded before the launch.',
  });
  const s = await apiCall<SessionRow>('POST', '/api/sessions', { project_id: projectId, agent: 'shell' });
  await page.goto(`${env.url}/sessions/${s.id}`);
  const toggle = page.getByRole('button', { name: 'Memory panel', exact: true });
  await expect(toggle).toBeVisible();
  if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click();
  const panel = page.getByRole('complementary', { name: 'Memory' });
  const injected = panel.locator('details.inj');
  await injected.locator('summary').click();
  await expect(injected).toContainText('blirp memory: Plain Folder');
  await expect(injected).toContainText('E2E seeded thread');
  await expect(panel.locator('article.rec', { hasText: 'E2E seeded thread' })).toBeVisible();
  await apiCall('POST', `/api/sessions/${s.id}/stop`);
});

test('distill with the none summarizer pauses distilling; subagents sit under their parent', async () => {
  // A Claude Code transcript with one subagent, as ingest finds it under CLAUDE_CONFIG_DIR.
  const sid = randomUUID();
  const start = Date.now() - 60 * 60_000;
  const common = { cwd: env.plain, sessionId: sid, version: '2.0.0', userType: 'external', entrypoint: 'cli' };
  const usage = { input_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 3 };
  const turn = (uuid: string, parent: string | null, at: number, role: 'user' | 'assistant', text: string, side?: string): string =>
    JSON.stringify({
      parentUuid: parent,
      isSidechain: side !== undefined,
      ...(side ? { agentId: side } : {}),
      type: role,
      message:
        role === 'user'
          ? { role, content: text }
          : { model: 'claude-haiku-4-5', id: `msg_${uuid}`, type: 'message', role, content: [{ type: 'text', text }], usage },
      uuid,
      timestamp: new Date(start + at).toISOString(),
      ...common,
    });
  const dir = join(env.userHome, '.claude', 'projects', 'e2e-plain');
  mkdirSync(join(dir, sid, 'subagents'), { recursive: true });
  writeFileSync(
    join(dir, sid, 'subagents', 'agent-e2e1.jsonl'),
    `${turn('su1', null, 2000, 'user', 'Find the notes file', 'e2e1')}\n${turn('sa1', 'su1', 3000, 'assistant', 'Found notes.txt', 'e2e1')}\n`,
  );
  writeFileSync(
    join(dir, `${sid}.jsonl`),
    `${turn('u1', null, 1000, 'user', 'Explain the notes file')}\n${turn('a1', 'u1', 4000, 'assistant', 'It holds two lines.')}\n`,
  );
  const findParent = async (): Promise<SessionRow | undefined> =>
    (await apiCall<{ items: SessionRow[] }>('GET', '/api/sessions?agent=claude&limit=200')).items.find(
      (x) => x.agent_session_id === sid,
    );
  await expect.poll(findParent, { timeout: 30_000 }).toBeTruthy();
  const parent = await findParent();
  if (!parent) throw new Error('the ingested session disappeared');

  await page.goto(`${env.url}/sessions/${parent.id}`);
  const sidebar = page.getByRole('complementary', { name: 'Sessions' });
  await expect(sidebar.locator(`a[href="/sessions/${parent.id}"]`)).toBeVisible();
  const expander = sidebar.getByRole('button', { name: '1 subagent' });
  await expect(expander).toBeVisible();
  await expander.click();
  const child = sidebar.locator('a.child');
  await expect(child).toHaveCount(1);

  const toggle = page.getByRole('button', { name: 'Memory panel', exact: true });
  if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click();
  const panel = page.getByRole('complementary', { name: 'Memory' });
  await panel.getByRole('button', { name: 'Distill now' }).click();
  await expect(page.getByText('Distill queued')).toBeVisible();
  // No summarizer is a failure of the summarizer, not of the session (§9): distilling pauses.
  const distill = async (): Promise<{ paused: string | null; reason: string | null }> =>
    (await apiCall<{ distill: { paused: string | null; reason: string | null } }>('GET', '/api/settings')).distill;
  await expect.poll(async () => (await distill()).paused).toBe('unavailable');
  expect((await distill()).reason).toContain('memory.summarizer = "none"');
  await expect(panel.getByTestId('distill-error')).toHaveCount(0);
});

test('sync: enabling the hub gives an invite with QR; a join link only prefills and asks first', async () => {
  await page.goto(`${env.url}/settings/sync`);
  await confirmNextDialog();
  await page.getByRole('button', { name: 'Enable hub' }).click();
  await expect(page.getByTestId('sync-role')).toHaveText('hub');
  // A settings save right after the role change keeps the role.
  const nameForm = page.locator('form.name');
  const machineName = nameForm.getByLabel('Machine name');
  const originalName = await machineName.inputValue();
  await machineName.fill(`${originalName}-hub`);
  await nameForm.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Machine name saved')).toBeVisible();
  expect((await apiCall<{ role: string }>('GET', '/api/sync/status')).role).toBe('hub');
  await machineName.fill(originalName);
  await nameForm.getByRole('button', { name: 'Save' }).click();
  await expect(nameForm.getByRole('button', { name: 'Save' })).toBeDisabled();
  await page.getByRole('button', { name: 'Create invite' }).click();
  await expect(page.getByTestId('pairing-code')).toHaveText(/^[2-9A-HJ-NP-Z]{4}-[2-9A-HJ-NP-Z]{4}$/);
  await expect(page.getByRole('img', { name: 'Pairing QR code' }).locator('canvas')).toBeVisible();
  await expect(page.getByLabel('Invite', { exact: true })).toHaveValue(/^blirp1-/);
  await expect(page.getByLabel('Join link', { exact: true })).toHaveValue(/^blirp:\/\/join\//);
  await expect(page.getByTestId('expiry')).toContainText(/Expires in (9|10)m/);

  const invite = await page.getByLabel('Invite', { exact: true }).inputValue();
  const code = (await page.getByTestId('pairing-code').textContent()) ?? '';

  await confirmNextDialog();
  await page.getByRole('button', { name: 'Disable hub' }).click();
  await expect(page.getByTestId('sync-role')).toHaveText('standalone');

  // A deep link (any web page can open one) prefills the form and never pairs on its own.
  await page.goto(`${env.url}/settings/sync?join=${invite}&code=${code}`);
  const joinForm = page.getByRole('form', { name: 'Join a hub' });
  await expect(joinForm.getByLabel(/^Invite/)).toHaveValue(invite);
  await expect(joinForm.getByLabel('Pairing code')).toHaveValue(code);
  await expect(page).toHaveURL(`${env.url}/settings/sync`); // the code does not stay in history
  const dialog = page.getByRole('dialog', { name: 'Pair with this hub?' });
  await expect(dialog).toHaveCount(0);
  await joinForm.getByRole('button', { name: 'Pair with hub' }).click();
  await expect(dialog.getByRole('alert')).toContainText('opened from outside blirp');
  // The invite names the hub it was created on: this machine.
  const me = await apiCall<{ machine_id: string }>('GET', '/api/sync/status');
  await expect(dialog.getByTestId('hub-fingerprint')).toHaveText(me.machine_id.match(/.{1,4}/g)?.join(' ') ?? '');
  const control = dialog.getByRole('checkbox', { name: 'Allow this hub to start and control terminals on this machine' });
  await expect(control).not.toBeChecked();
  await dialog.getByRole('button', { name: 'Cancel' }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByTestId('sync-role')).toHaveText('standalone');
});

test('agents: integration rows, and install/uninstall round trip in the temp home', async () => {
  await page.goto(`${env.url}/settings/agents`);
  await expect(page.locator('li.agent[data-agent="shell"]').getByTestId('hooks-state')).toHaveText('not supported');
  const claude = page.locator('li.agent[data-agent="claude"]');
  await expect(claude.getByTestId('hooks-state')).toHaveText('not installed');
  await expect(claude.getByTestId('mcp-state')).toHaveText('not installed');

  await claude.getByRole('button', { name: 'Install', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Install global integration' });
  await expect(dialog).toContainText('~/.claude/settings.json');
  await dialog.getByRole('button', { name: 'Install', exact: true }).click();
  await expect(claude.getByTestId('hooks-state')).toHaveText('installed');
  await expect(claude.getByTestId('mcp-state')).toHaveText('installed');
  // The daemon runs with CLAUDE_CONFIG_DIR in the temp home (global-setup.ts).
  const settingsFile = join(env.userHome, '.claude', 'settings.json');
  expect(readFileSync(settingsFile, 'utf8')).toContain('hook claude');

  await confirmNextDialog();
  await claude.getByRole('button', { name: 'Uninstall' }).click();
  await expect(claude.getByTestId('hooks-state')).toHaveText('not installed');
  await expect(claude.getByTestId('mcp-state')).toHaveText('not installed');
  if (existsSync(settingsFile)) expect(readFileSync(settingsFile, 'utf8')).not.toContain('hook claude');
});

test('agents: a pasted claude login token is stored, never shown back, and removable', async () => {
  const secret = 'e2e-login-token-not-real';
  const file = join(env.root, 'home', 'secrets', 'claude_oauth_token');
  await page.goto(`${env.url}/settings/agents`);
  const claude = page.locator('li.agent[data-agent="claude"]');
  await expect(claude.getByTestId('token-state')).toHaveText('none');
  const input = claude.getByLabel('Claude login token');
  await expect(input).toHaveAttribute('type', 'password');
  await input.fill(secret);
  await claude.getByRole('button', { name: 'Save token' }).click();
  await expect(claude.getByTestId('token-state')).toHaveText('stored');
  await expect(input).toHaveValue('');
  expect(readFileSync(file, 'utf8')).toBe(secret);
  await expect(page.locator('body')).not.toContainText(secret);

  await confirmNextDialog();
  await claude.getByRole('button', { name: 'Remove token' }).click();
  await expect(claude.getByTestId('token-state')).toHaveText('none');
  expect(existsSync(file)).toBe(false);
});

test('command palette jumps to a project', async () => {
  await page.goto(`${env.url}/sessions`);
  await page.keyboard.press('ControlOrMeta+k');
  const palette = page.getByRole('dialog', { name: 'Command palette' });
  await expect(palette).toBeVisible();
  await palette.getByRole('combobox').fill('git-repo');
  await page.keyboard.press('Enter');
  await expect(palette).toBeHidden();
  await expect(page).toHaveURL(new RegExp(`${repoProjectUrl}$`));
});

test('mobile width (390px) keeps sessions and project pages inside the viewport', async () => {
  await page.setViewportSize({ width: 390, height: 844 });
  const overflow = (): Promise<number> =>
    page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

  await page.goto(`${env.url}/sessions/${sessionId}`);
  await expect(page.getByRole('heading', { name: 'Transcript' })).toBeVisible();
  expect(await overflow()).toBeLessThanOrEqual(0);
  await page.getByRole('button', { name: 'Show sessions list' }).click();
  await expect(page.getByRole('complementary', { name: 'Sessions' }).locator(`a[href="/sessions/${sessionId}"]`)).toBeInViewport();
  // The open drawer (320px) covers the scrim's middle; tap the strip beside it, as a finger would.
  const scrim = page.getByRole('button', { name: 'Close sessions list' });
  const box = await scrim.boundingBox();
  if (!box) throw new Error('the sessions scrim is not laid out');
  await scrim.click({ position: { x: box.width - 8, y: box.height / 2 } });
  await expect(scrim).toHaveCount(0);

  for (const path of [plainProjectUrl, `${plainProjectUrl}/memory`, `${repoProjectUrl}/git`, '/projects', '/settings/memory']) {
    await page.goto(`${env.url}${path}`);
    await expect(page.locator('.page-title').first()).toBeVisible();
    expect(await overflow(), path).toBeLessThanOrEqual(0);
  }
  await page.setViewportSize({ width: 1360, height: 860 });
});

test('health reports full capabilities to the local client', async () => {
  const health = await apiCall<{ capabilities: { admin: boolean; control_terminals: boolean; local: boolean; files: boolean } }>(
    'GET',
    '/api/health',
  );
  expect(health.capabilities).toEqual({ admin: true, control_terminals: true, local: true, files: true });
});

test('deletes an ended session here, and follows a delete made by another client', async () => {
  const mine = await endedShell({ project_id: idOf(plainProjectUrl) });
  const other = await endedShell({ project_id: idOf(plainProjectUrl) });
  await page.goto(`${env.url}/sessions/${mine.id}`);
  const sidebar = page.getByRole('complementary', { name: 'Sessions' });
  await expect(sidebar.locator(`a[href="/sessions/${mine.id}"]`)).toBeVisible();
  await expect(sidebar.locator(`a[href="/sessions/${other.id}"]`)).toBeVisible();

  await confirmNextDialog();
  await page.getByRole('button', { name: 'Delete session' }).click();
  await expect(page.getByText('Session deleted')).toBeVisible();
  await expect(page).toHaveURL(`${env.url}/sessions`);
  await expect(sidebar.locator(`a[href="/sessions/${mine.id}"]`)).toHaveCount(0);
  expect((await page.request.get(`${env.url}/api/sessions/${mine.id}`, { headers: AUTH })).status()).toBe(404);

  // Another client deletes: the `session_deleted` event removes the card here.
  await apiCall('DELETE', `/api/sessions/${other.id}`);
  await expect(sidebar.locator(`a[href="/sessions/${other.id}"]`)).toHaveCount(0);

  // Live sessions offer no delete (the daemon would answer 409 `session_live`).
  await page.goto(`${env.url}/sessions/${sessionId}`);
  await page.getByRole('button', { name: 'Resume' }).click();
  await expect(page.getByRole('button', { name: 'Stop' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Delete session' })).toHaveCount(0);
  await confirmNextDialog();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(page.getByTestId('terminal-exit')).toContainText(STOPPED);
});

test('removes a dirty session worktree only after an explicit force', async () => {
  const s = await endedShell({ project_id: idOf(repoProjectUrl), worktree: true });
  const wt = s.worktree;
  if (!wt) throw new Error('the session got no worktree');
  expect(existsSync(wt)).toBe(true);
  writeFileSync(join(wt, 'scratch.txt'), 'uncommitted\n');

  await page.goto(`${env.url}/sessions/${s.id}`);
  await confirmNextDialog();
  await page.getByRole('button', { name: 'Remove worktree' }).click();
  const dialog = page.getByRole('dialog', { name: 'Uncommitted changes' });
  await expect(dialog).toContainText('uncommitted change');
  // The browser logs the expected 409 `worktree_dirty` as a failed resource load.
  consoleErrors.splice(0, consoleErrors.length, ...consoleErrors.filter((e) => !e.includes('status of 409')));
  expect(existsSync(wt)).toBe(true);
  await dialog.getByRole('button', { name: 'Force remove' }).click();
  await expect(page.getByText('Worktree removed')).toBeVisible();
  await expect(dialog).toBeHidden();
  await expect(page.getByRole('button', { name: 'Remove worktree' })).toHaveCount(0);
  expect((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).worktree).toBeNull();
  await expect.poll(() => existsSync(wt)).toBe(false);
});

test('memory injection toggle and per-agent opt-out persist; summarizer state shows', async () => {
  interface MemoryView {
    config: { memory: { inject: boolean; inject_disabled_agents: string[] } };
  }
  const injectBox = (): ReturnType<Page['getByRole']> =>
    page.getByRole('checkbox', { name: /Give new sessions this project.s memory/ });
  const shellBox = (): ReturnType<Page['getByRole']> =>
    page.getByRole('group', { name: 'Inject memory for these agents' }).getByRole('checkbox', { name: 'Shell', exact: true });
  const memoryConfig = async (): Promise<MemoryView['config']['memory']> =>
    (await apiCall<MemoryView>('GET', '/api/settings')).config.memory;

  await page.goto(`${env.url}/settings/memory`);
  const status = page.getByTestId('distill-status');
  await expect(status).toContainText('Automatic distilling is paused'); // by the distill test above
  await expect(status).toContainText('memory.summarizer = "none"');
  await expect(status).toContainText('resumes by itself');
  await expect(status).toContainText('Distill jobs today');
  await shellBox().uncheck();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Memory settings saved')).toBeVisible();
  await page.reload();
  await expect(shellBox()).not.toBeChecked();
  expect((await memoryConfig()).inject_disabled_agents).toEqual(['shell']);

  await injectBox().uncheck();
  await expect(page.getByRole('group', { name: 'Inject memory for these agents' })).toHaveCount(0);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect.poll(async () => (await memoryConfig()).inject).toBe(false);
  await page.reload();
  await expect(injectBox()).not.toBeChecked();
  expect((await memoryConfig()).inject_disabled_agents).toEqual(['shell']);

  // Back to the defaults for anything that runs later.
  await injectBox().check();
  await shellBox().check();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect.poll(memoryConfig).toEqual(expect.objectContaining({ inject: true, inject_disabled_agents: [] }));
});

test('the automatic summarizer says what it uses and why', async () => {
  // Answered here: the real route probes the claude and codex logins on this machine.
  const route = '**/api/settings/summarizer';
  await page.route(route, (r) =>
    r.fulfill({ json: { backend: 'claude', model: 'sonnet', default_agent: 'codex', fallback: 'not_logged_in' } }),
  );
  await page.goto(`${env.url}/settings/memory`);
  await expect(page.getByTestId('auto-summarizer')).toHaveCount(0);
  await page.getByRole('combobox', { name: /^Summarizer/ }).selectOption('auto');
  await expect(page.getByTestId('auto-summarizer')).toHaveText(
    'Automatic uses Claude Code (Sonnet): your default agent, Codex, is not signed in.',
  );
  await page.unroute(route);
});

test('creates a project without a folder; its session runs in the blirp workspace with memory', async () => {
  await page.goto(`${env.url}/projects`);
  await page.getByRole('button', { name: 'New project' }).click();
  const dialog = page.getByRole('dialog', { name: 'New project' });
  await dialog.getByLabel('Name', { exact: true }).fill('Design notes');
  await dialog.getByLabel(/^Brief/).fill('Screens live in the design tool, reached through its MCP server.');
  await dialog.getByRole('button', { name: 'Create project' }).click();
  await expect(page).toHaveURL(/\/projects\/[^/]+$/);
  const projectId = new URL(page.url()).pathname.split('/').pop() ?? '';
  await expect(page.getByRole('heading', { level: 1, name: 'Design notes' })).toBeVisible();
  const workspace = page.getByTestId('workspace');
  await expect(workspace).toContainText('blirp workspace');
  await expect(workspace).toContainText(projectId);
  // The projects list shows it without a "missing folder" warning.
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  const card = page.locator('.pcard', { hasText: 'Design notes' });
  await expect(card).toContainText('No folder · sessions start in a blirp workspace');
  await card.click();

  await page.locator('.head').getByRole('button', { name: 'New session' }).click();
  const ns = page.getByRole('dialog', { name: 'New session' });
  await expect(ns.getByLabel('Project')).toHaveValue(projectId);
  await expect(ns.getByTestId('workspace-hint')).toContainText(projectId);
  await ns.getByLabel('Agent').selectOption('shell');
  await page.waitForTimeout(500);
  await ns.getByRole('button', { name: 'Start session' }).click();
  await expect(page).toHaveURL(/\/sessions\/[^/]+$/);
  const id = new URL(page.url()).pathname.split('/').pop() ?? '';
  const session = await apiCall<{ cwd: string; project_id: string }>('GET', `/api/sessions/${id}`);
  expect(session.project_id).toBe(projectId);
  const dir = join(env.root, 'home', 'workspaces', projectId);
  expect(existsSync(dir)).toBe(true);
  expect(session.cwd.toLowerCase()).toContain(join('workspaces', projectId).toLowerCase());
  // The shell really runs there.
  await runInTerminal(id, isWindows ? '(Get-Location).Path' : 'pwd', projectId);

  // The memory panel has the project's brief, as for any project.
  const toggle = page.getByRole('button', { name: 'Memory panel', exact: true });
  if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click();
  const panel = page.getByRole('complementary', { name: 'Memory' });
  await expect(panel).toContainText('Screens live in the design tool');
  await expect(panel.getByTestId('chats-memory')).toHaveCount(0);

  await confirmNextDialog();
  await page.getByRole('button', { name: 'Stop' }).click();
  await expect(page.getByTestId('terminal-exit')).toContainText(STOPPED);
});

test('a session in no project is a chat, kept out of projects, and moves into one', async () => {
  const projectsBefore = await apiCall<{ chats: boolean }[]>('GET', '/api/projects');
  // The home folder is no project: a session there is a chat.
  const chat = await apiCall<{ id: string; project_id: string }>('POST', '/api/sessions', { cwd: env.userHome, agent: 'shell' });
  await page.goto(`${env.url}/sessions/${chat.id}`);
  const sidebar = page.getByRole('complementary', { name: 'Sessions' });
  const chats = sidebar.getByRole('region', { name: 'Chats' });
  await expect(chats.locator(`a[href="/sessions/${chat.id}"]`)).toBeVisible();
  await expect(page.getByRole('navigation', { name: 'Breadcrumb' })).toContainText('Chats');
  const toggle = page.getByRole('button', { name: 'Memory panel', exact: true });
  if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click();
  await expect(page.getByTestId('chats-memory')).toBeVisible();
  // Chats is no project card.
  await page.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  const realCount = (await apiCall<{ chats: boolean }[]>('GET', '/api/projects')).filter((p) => !p.chats).length;
  expect(realCount).toBe(projectsBefore.filter((p) => !p.chats).length);
  await expect(page.locator('.pcard')).toHaveCount(realCount);
  await expect(page.locator('.pcard', { hasText: 'Chats' })).toHaveCount(0);

  await page.goto(`${env.url}/sessions/${chat.id}`);
  await page.getByRole('button', { name: 'Move to project' }).click();
  const dialog = page.getByRole('dialog', { name: 'Move session' });
  await dialog.getByRole('combobox').selectOption({ label: 'Design notes' });
  await dialog.getByRole('button', { name: 'Move' }).click();
  await expect(page.getByText('Moved to Design notes')).toBeVisible();
  await expect(sidebar.getByRole('region', { name: 'Design notes' }).locator(`a[href="/sessions/${chat.id}"]`)).toBeVisible();
  const moved = await apiCall<{ project_id: string }>('GET', `/api/sessions/${chat.id}`);
  expect(moved.project_id).not.toBe(chat.project_id);
  await apiCall('POST', `/api/sessions/${chat.id}/stop`);
});

test('new session folder picker lists folders only, hidden ones on request', async () => {
  await page.goto(`${env.url}/sessions`);
  await page.getByRole('button', { name: 'New session' }).first().click();
  const dialog = page.getByRole('dialog', { name: 'New session' });
  await dialog.getByRole('radio', { name: 'Folder path' }).click();
  await dialog.getByRole('button', { name: 'Browse…' }).click();
  const list = dialog.getByRole('list', { name: /^Folders on / });
  await expect(list.getByRole('button', { name: 'code' })).toBeVisible();
  await expect(list.getByRole('button', { name: '.claude' })).toHaveCount(0);
  await dialog.getByRole('checkbox', { name: 'Hidden' }).check();
  await expect(list.getByRole('button', { name: '.claude' })).toBeVisible();
  await list.getByRole('button', { name: 'code' }).click();
  await expect(list.getByRole('button', { name: 'demo-repo' })).toBeVisible();
  await list.getByRole('button', { name: 'demo-repo' }).click();
  await dialog.getByRole('button', { name: 'Use this folder' }).click();
  await expect(dialog.getByLabel('Folder', { exact: true })).toHaveValue(/demo-repo$/);
  // Outside the home folder nothing is listed.
  const me = await apiCall<{ machine: { id: string } }>('GET', '/api/health');
  const outside = await page.request.get(`${env.url}/api/machines/${me.machine.id}/dirs?path=${encodeURIComponent(env.root)}`, {
    headers: AUTH,
  });
  expect(outside.status()).toBe(403);
  await dialog.getByRole('button', { name: 'Cancel' }).click();
});

interface NoteCall {
  title: string;
  body: string;
}

test('notifications: OS notification in the background, in-app toast and title badge in front', async ({ browser }) => {
  // Its own browser with notification permission and a recording Notification stub, and a
  // switch for whether the window counts as focused.
  const ctx = await browser.newContext({ permissions: ['notifications'] });
  await ctx.addInitScript(() => {
    const w = window as unknown as { __notes: { title: string; body: string; click: () => void }[]; __focus: boolean; __csp: string[] };
    w.__notes = [];
    w.__focus = true;
    w.__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => w.__csp.push(`${e.violatedDirective} ${e.blockedURI}`));
    class FakeNotification {
      static permission = 'granted';
      static requestPermission(): Promise<string> {
        return Promise.resolve('granted');
      }
      onclick: (() => void) | null = null;
      constructor(title: string, opts?: { body?: string }) {
        w.__notes.push({ title, body: opts?.body ?? '', click: () => this.onclick?.() });
      }
      close(): void {}
    }
    Object.defineProperty(window, 'Notification', { value: FakeNotification, configurable: true });
    document.hasFocus = () => w.__focus;
  });
  const p = await ctx.newPage();
  const errors: string[] = [];
  p.on('pageerror', (e) => errors.push(e.message));
  const notes = (): Promise<NoteCall[]> =>
    p.evaluate(() => (window as unknown as { __notes: NoteCall[] }).__notes.map(({ title, body }) => ({ title, body })));
  const setFocus = (on: boolean): Promise<void> =>
    p.evaluate((f) => {
      (window as unknown as { __focus: boolean }).__focus = f;
      window.dispatchEvent(new Event(f ? 'focus' : 'blur'));
    }, on);

  await p.goto(`${env.url}/settings/appearance#token=${env.token}`);
  const section = p.getByRole('region', { name: 'Notifications' });
  await expect(section.getByRole('checkbox', { name: /needs attention/ })).toBeChecked();
  await expect(section.getByRole('checkbox', { name: 'Play a sound' })).not.toBeChecked();
  await section.getByRole('button', { name: 'Send test notification' }).click();
  await expect(section.getByRole('status')).toContainText('Sent to the browser.');
  expect((await notes())[0]?.title).toBe('blirp');

  // An external Claude Code session reported by its hooks.
  const cwd = join(env.root, 'notify-proj');
  mkdirSync(cwd, { recursive: true });
  const asid = randomUUID();
  const hook = (event: string): Promise<{ session_id: string | null }> =>
    apiCall('POST', `/api/hooks/claude/${event}`, { cwd, payload: { session_id: asid, cwd } });
  const sid = (await hook('SessionStart')).session_id ?? '';
  expect(sid).not.toBe('');

  // In the background: an OS notification and a title count.
  await setFocus(false);
  await hook('Notification');
  await expect.poll(async () => (await notes()).length).toBe(2);
  // The body names where the session is filed once the UI has loaded it: a session outside blirp in
  // the temp folder is a chat.
  expect((await notes())[1]).toEqual({ title: 'Claude Code in notify-proj', body: expect.stringMatching(/^Needs your input( · Chats)?$/) });
  await expect(p).toHaveTitle('(1) blirp');
  const icon = p.locator('link[rel="icon"]').first();
  await expect(icon).toHaveAttribute('href', /^data:image\/png/);

  // Clicking it opens the session; back in front, the badge clears.
  await setFocus(true);
  await expect(p).toHaveTitle('(1) blirp'); // still on Settings: not seen yet
  await p.evaluate(() => (window as unknown as { __notes: { click: () => void }[] }).__notes[1]?.click());
  await expect(p).toHaveURL(`${env.url}/sessions/${sid}`);
  await expect(p).toHaveTitle('blirp');
  await expect(icon).not.toHaveAttribute('href', /^data:/);

  // In front, looking at that session: nothing at all.
  await hook('UserPromptSubmit');
  await hook('Notification');
  await p.waitForTimeout(1000);
  expect(await notes()).toHaveLength(2);
  await expect(p.getByRole('status').filter({ hasText: 'needs your input' })).toHaveCount(0);

  // In front, on another page: an in-app toast with Open, no OS notification.
  await p.getByRole('navigation', { name: 'Main' }).getByRole('link', { name: 'Projects' }).click();
  await hook('UserPromptSubmit');
  await hook('Notification');
  const toast = p.getByRole('status').filter({ hasText: 'Claude Code in notify-proj needs your input' });
  await expect(toast).toBeVisible();
  expect(await notes()).toHaveLength(2);
  await toast.getByRole('button', { name: 'Open' }).click();
  await expect(p).toHaveURL(`${env.url}/sessions/${sid}`);

  // Turned off: nothing, even in the background (the reload starts a fresh stub).
  await p.goto(`${env.url}/settings/appearance`);
  await section.getByRole('checkbox', { name: /needs attention/ }).uncheck();
  await setFocus(false);
  await hook('UserPromptSubmit');
  await hook('Notification');
  await p.waitForTimeout(1000);
  expect(await notes()).toHaveLength(0);
  await expect(p).toHaveTitle('blirp');

  expect(await p.evaluate(() => (window as unknown as { __csp: string[] }).__csp)).toEqual([]);
  expect(errors).toEqual([]);
  await ctx.close();
});

test('notifications: inside the desktop app they go through its notify command', async ({ browser }) => {
  const ctx = await browser.newContext();
  await ctx.addInitScript(() => {
    const w = window as unknown as { __calls: unknown[]; __TAURI_INTERNALS__: unknown };
    w.__calls = [];
    w.__TAURI_INTERNALS__ = {
      invoke: (cmd: string, args: unknown) => {
        w.__calls.push({ cmd, args });
        return Promise.resolve({ backend: 'Windows notifications', problem: null });
      },
    };
  });
  const p = await ctx.newPage();
  await p.goto(`${env.url}/settings/appearance#token=${env.token}`);
  const section = p.getByRole('region', { name: 'Notifications' });
  await expect(section).toContainText('a system notification from the desktop app');
  await expect(section.getByRole('button', { name: 'Enable desktop notifications' })).toHaveCount(0);
  await section.getByRole('button', { name: 'Send test notification' }).click();
  await expect(section.getByRole('status')).toContainText('Sent to Windows notifications.');
  const calls = await p.evaluate(() => (window as unknown as { __calls: unknown[] }).__calls);
  expect(calls).toEqual([{ cmd: 'notify', args: { title: 'blirp', body: expect.stringContaining('Test notification') } }]);
  await ctx.close();
});

test('sessions list: recent activity first, every machine labeled and filterable', async () => {
  // Two external Claude Code transcripts: one started hours ago but active a minute ago, one
  // started later that went quiet. Most recent activity comes first, not the latest start.
  const folder = join(env.root, 'order-e2e');
  mkdirSync(folder, { recursive: true });
  // Registered, so the temp folder is a project of its own rather than Home.
  await apiCall('POST', '/api/projects', { path: folder, name: 'order-e2e' });
  const now = Date.now();
  const transcript = (sid: string, prompt: string, first: number, last: number): string => {
    const common = { cwd: folder, sessionId: sid, version: '2.0.0', userType: 'external', entrypoint: 'cli' };
    const usage = { input_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 3 };
    return [
      { parentUuid: null, type: 'user', message: { role: 'user', content: prompt }, uuid: `${sid}-u`, timestamp: new Date(first).toISOString() },
      {
        parentUuid: `${sid}-u`,
        type: 'assistant',
        message: { model: 'claude-haiku-4-5', id: `msg_${sid}`, type: 'message', role: 'assistant', content: [{ type: 'text', text: 'ok' }], usage },
        uuid: `${sid}-a`,
        timestamp: new Date(last).toISOString(),
      },
    ]
      .map((t) => JSON.stringify({ ...t, isSidechain: false, ...common }))
      .join('\n');
  };
  const [longRunning, recentStart] = [randomUUID(), randomUUID()];
  const dir = join(env.userHome, '.claude', 'projects', 'e2e-order');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${longRunning}.jsonl`), `${transcript(longRunning, 'Long running order check', now - 3 * 3_600_000, now - 5 * 60_000)}\n`);
  writeFileSync(join(dir, `${recentStart}.jsonl`), `${transcript(recentStart, 'Recently started order check', now - 30 * 60_000, now - 20 * 60_000)}\n`);
  const ingested = async (): Promise<number> =>
    (await apiCall<{ items: SessionRow[] }>('GET', '/api/sessions?agent=claude&limit=500')).items.filter(
      (x) => x.agent_session_id === longRunning || x.agent_session_id === recentStart,
    ).length;
  await expect.poll(ingested, { timeout: 30_000 }).toBe(2);

  // Pretend this machine is paired with a hub that has one session of its own.
  const me = await apiCall<{ machine: { id: string; name: string } }>('GET', '/api/health');
  // The hub went offline with a session it last reported as working.
  const hub = { id: 'e2e-hub', name: 'Studio Mac', os: 'macos', role: 'hub', last_seen: now - 600_000, revoked: false, online: false };
  const self = { ...me.machine, os: 'windows', role: 'node', last_seen: now, revoked: false, online: true };
  await page.route(`${env.url}/api/sync/status`, (r) =>
    r.fulfill({
      json: {
        role: 'node',
        machine_id: me.machine.id,
        hub: hub.id,
        connected: true,
        last_sync_at: now,
        pending_outbox: 0,
        portal_url: null,
        portal_cert_fingerprint: null,
      },
    }),
  );
  await page.route(`${env.url}/api/machines`, (r) => r.fulfill({ json: [self, hub] }));
  await page.route(`${env.url}/api/machines/${hub.id}/health`, (r) => r.fulfill({ json: { keep_awake: false } }));
  const [template] = (await apiCall<{ items: Array<Record<string, unknown>> }>('GET', '/api/sessions?limit=1')).items;
  const remoteSession = {
    ...template,
    id: 'e2e-remote',
    machine_id: hub.id,
    project_id: 'e2e-remote-project',
    title: 'Remote order check',
    status: 'working',
    origin: 'external',
    parent_session_id: null,
    worktree: null,
    last_activity_at: now - 5 * 60_000,
  };
  await page.route(
    (u) => u.pathname === '/api/sessions',
    async (r) => {
      const res = await r.fetch();
      const body = (await res.json()) as { items: Array<Record<string, unknown>>; next_cursor: string | null };
      const u = new URL(r.request().url());
      const machine = u.searchParams.get('machine');
      const fresh = !u.searchParams.has('cursor') && !u.searchParams.has('parent') && !u.searchParams.has('q');
      if (fresh && (machine === null || machine === hub.id)) body.items.push(remoteSession);
      await r.fulfill({ response: res, json: body });
    },
  );
  await page.goto(`${env.url}/sessions`);
  const sidebar = page.getByRole('complementary', { name: 'Sessions' });
  const group = sidebar.getByRole('region', { name: 'order-e2e' });
  await expect(group.locator('.scard .title')).toHaveText(['Long running order check', 'Recently started order check']);
  // This machine's sessions carry its name once another machine is paired, the hub's carry the hub's.
  await expect(group.getByTestId('machine-badge').first()).toHaveText(me.machine.name);
  const remote = sidebar.locator('a[href="/sessions/e2e-remote"]');
  await expect(remote.getByTestId('machine-badge')).toHaveText(hub.name);
  // Its last report is not trusted: the chip says the machine is offline.
  await expect(remote.locator('.chip')).toHaveText('Offline');

  const pick = sidebar.getByRole('combobox', { name: 'Machine' });
  await pick.selectOption(hub.id);
  await expect(remote).toBeVisible();
  await expect(group).toHaveCount(0);
  await pick.selectOption(me.machine.id);
  await expect(group.locator('.scard .title')).toHaveText(['Long running order check', 'Recently started order check']);
  await expect(remote).toHaveCount(0);

  await page.unrouteAll({ behavior: 'wait' });
  await page.goto(`${env.url}/sessions`);
});

test('a running session whose agent compacted its context suggests a fresh session, once per compaction', async () => {
  // A live Claude Code session started outside blirp: its transcript changed a moment ago and
  // holds a compaction (the boundary marker and the summary Claude writes in place of older turns).
  const folder = join(env.root, 'compact-e2e');
  mkdirSync(folder, { recursive: true });
  await apiCall('POST', '/api/projects', { path: folder, name: 'compact-e2e' });
  const sid = randomUUID();
  const common = { cwd: folder, sessionId: sid, version: '2.0.0', userType: 'external', entrypoint: 'cli', isSidechain: false };
  const usage = { input_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 3 };
  const lines = (tag: string, at: number): string[] => [
    { type: 'user', message: { role: 'user', content: `Keep refactoring ${tag}` }, uuid: `${tag}-u`, timestamp: new Date(at).toISOString() },
    {
      type: 'system',
      subtype: 'compact_boundary',
      content: 'Conversation compacted',
      compactMetadata: { trigger: 'auto', preTokens: 190_000 },
      uuid: `${tag}-b`,
      timestamp: new Date(at + 1_000).toISOString(),
    },
    {
      type: 'user',
      isCompactSummary: true,
      message: { role: 'user', content: `Summary of the work on ${tag}` },
      uuid: `${tag}-s`,
      timestamp: new Date(at + 2_000).toISOString(),
    },
    {
      type: 'assistant',
      message: { model: 'claude-haiku-4-5', id: `msg_${tag}`, type: 'message', role: 'assistant', content: [{ type: 'text', text: 'ok' }], usage },
      uuid: `${tag}-a`,
      timestamp: new Date(at + 3_000).toISOString(),
    },
  ].map((l) => JSON.stringify({ ...l, parentUuid: null, ...common }));
  const dir = join(env.userHome, '.claude', 'projects', 'e2e-compact');
  mkdirSync(dir, { recursive: true });
  const file = join(dir, `${sid}.jsonl`);
  writeFileSync(file, `${lines('first', Date.now() - 30_000).join('\n')}\n`);
  interface Compacted extends SessionRow {
    compacted_at: number | null;
  }
  const find = async (): Promise<Compacted | undefined> =>
    (await apiCall<{ items: Compacted[] }>('GET', '/api/sessions?agent=claude&limit=500')).items.find((x) => x.agent_session_id === sid);
  await expect.poll(async () => (await find())?.compacted_at ?? null, { timeout: 30_000 }).not.toBeNull();
  const session = await find();
  if (!session) throw new Error('the ingested session disappeared');
  expect(LIVE).toContain(session.status);

  await page.goto(`${env.url}/sessions/${session.id}`);
  const hint = page.getByTestId('compaction-hint');
  await expect(hint).toContainText('The agent compacted its context');
  const start = hint.getByRole('button', { name: 'Start new session from this session' });

  // The launch waits while the daemon summarizes the session: both buttons say so and hold off.
  let release: () => void = () => undefined;
  const held = new Promise<void>((r) => (release = r));
  let launches = 0;
  await page.route(
    (u) => u.pathname === '/api/sessions',
    async (r) => {
      if (r.request().method() !== 'POST') return r.fallback();
      launches++;
      expect(r.request().postDataJSON()).toEqual({ continue_from: session.id, agent: 'claude' });
      await held;
      await r.fulfill({ status: 409, json: { error: { code: 'e2e_refused', message: 'e2e refused the launch' } } });
    },
  );
  const logged = consoleErrors.length;
  await start.click();
  const toolbar = page.getByRole('toolbar', { name: 'Session actions' });
  await expect(toolbar.getByRole('button', { name: 'Summarizing session…' })).toBeDisabled();
  await expect(hint.getByRole('button', { name: 'Summarizing session…' })).toBeDisabled();
  release();
  await expect(page.getByText('e2e refused the launch')).toBeVisible();
  await expect(start).toBeEnabled();
  expect(launches).toBe(1);
  await page.unrouteAll({ behavior: 'wait' });
  // The browser logs the refusal this test staged; nothing else may be logged meanwhile.
  expect(consoleErrors.splice(logged)).toEqual(['Failed to load resource: the server responded with a status of 409 (Conflict)']);

  // Dismissed: gone, also after a reload, until the agent compacts again.
  await hint.getByRole('button', { name: 'Dismiss' }).click();
  await expect(hint).toHaveCount(0);
  await page.reload();
  await expect(page.getByRole('toolbar', { name: 'Session actions' })).toBeVisible();
  await expect(hint).toHaveCount(0);
  writeFileSync(file, `${lines('second', Date.now() - 5_000).join('\n')}\n`, { flag: 'a' });
  await expect(hint).toBeVisible({ timeout: 30_000 });
});

test('files on hub: first-run banner, mode toggle, preview and a conflict badge', async () => {
  const secret = join(env.repo, '.env');
  const conflict = join(env.repo, 'notes.conflict-laptop-20260101-000000.md');
  writeFileSync(secret, 'API_TOKEN=e2e-placeholder\n');
  writeFileSync(conflict, 'kept by an earlier concurrent edit\n');
  await apiCall('POST', '/api/sync/hub/enable');
  await page.goto(`${env.url}${repoProjectUrl}/hub-files`);
  // Uploads wait for the first-run grace period; the banner says what will go where.
  const banner = page.getByTestId('files-banner');
  await expect(banner).toContainText(/Uploading \d+ project folders? .*to /);

  const modes = page.getByTestId('files-mode');
  await modes.getByRole('radio', { name: 'Off' }).click();
  await expect(modes.getByRole('radio', { name: 'Off' })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByTestId('files-effective')).toContainText('Uploads stopped');
  await modes.getByRole('radio', { name: 'Default' }).click();
  await expect(page.getByTestId('files-effective')).toContainText('Follows each machine');

  // The preview is a local dry run and names what stays behind.
  const root = page.getByTestId('files-root').first();
  await root.getByRole('button', { name: 'Preview' }).click();
  const preview = page.getByTestId('files-preview');
  await expect(preview).toContainText('would upload');
  await preview.getByText('Secrets: 1').click();
  await expect(preview).toContainText('.env');

  const region = page.getByRole('region', { name: 'Project file sync' });
  await region.getByRole('button', { name: 'Upload now' }).click();
  await expect(region).toHaveCount(0);
  await expect(page.getByTestId('conflict-badge')).toHaveText('1 conflict copy', { timeout: 30_000 });
  await expect(root.getByTestId('files-state')).toHaveText('Up to date');

  await apiCall('POST', '/api/sync/hub/disable');
  rmSync(secret);
  rmSync(conflict);
});

test('no CSP violations or unexpected console errors', async () => {
  const csp = await page.evaluate(() => (window as unknown as { __csp: string[] }).__csp);
  expect(csp).toEqual([]);
  expect(consoleErrors).toEqual([]);
});
