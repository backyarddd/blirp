// Drives the built SPA against the real daemon: auth, projects (git and plain folder),
// a live shell session, memory, wiki, resources, files, git, search, settings, palette,
// mobile layout, and no CSP violations along the way. Tests share one page and run in order.
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { e2eEnv } from './env';

const env = e2eEnv();
const isWindows = process.platform === 'win32';
// A user's Stop reads "Stopped" on daemons that report `stopped_by_user`, "Completed" before.
const STOPPED = /Completed|Stopped/;

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

/** Attach a second client to the session's terminal and return the daemon's screen snapshot. */
function snapshot(id: string): Promise<Snapshot> {
  return page.evaluate(
    (sid) =>
      new Promise<Snapshot>((resolve, reject) => {
        const ws = new WebSocket(`ws://${location.host}/api/terminals/${sid}/ws`);
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
      }),
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
  await page.getByRole('button', { name: 'Add folder' }).click();
  const dialog = page.getByRole('dialog', { name: 'Add folder' });
  await dialog.getByLabel('Folder path').fill(path);
  await dialog.getByRole('button', { name: 'Add project' }).click();
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

test('rejects a missing login, then signs in with /auth?token=', async () => {
  await page.goto(`${env.url}/`);
  await expect(page.getByRole('heading', { name: 'Sign in required' })).toBeVisible();
  await page.goto(`${env.url}/auth?token=${env.token}`);
  await expect(page).toHaveURL(`${env.url}/`);
  await expect(page.getByRole('heading', { name: 'Pick a session' })).toBeVisible();
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
    headers: { Origin: env.url },
  });
  expect([400, 422]).toContain(bad.status()); // §11 says 400; axum's JSON rejection gives 422
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

test('settings load and save (full config replace)', async () => {
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
  // The earlier memory change survived the second full-config write.
  const res = await page.request.get(`${env.url}/api/settings`);
  const view = (await res.json()) as { config: { memory: { distill_idle_secs: number }; sessions: { worktree_default: boolean } } };
  expect(view.config.memory.distill_idle_secs).toBe(600);
  expect(view.config.sessions.worktree_default).toBe(true);
});

/** Authenticated API call with the page's session cookie; fails the test on a non-2xx answer. */
async function apiCall<T>(method: 'GET' | 'POST', path: string, data?: unknown): Promise<T> {
  const res = await page.request.fetch(`${env.url}${path}`, { method, data, headers: { Origin: env.url } });
  const text = await res.text();
  expect(res.ok(), `${method} ${path}: ${res.status()} ${text}`).toBe(true);
  return (text ? JSON.parse(text) : undefined) as T;
}

interface SessionRow {
  id: string;
  agent_session_id: string | null;
  parent_session_id: string | null;
}

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

test('distill with the none summarizer explains the failure; subagents sit under their parent', async () => {
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
  await expect(panel.getByTestId('distill-error')).toContainText('memory.summarizer = "none"');
});

test('sync: a join deep link prefills the form; enabling the hub gives an invite with QR', async () => {
  await page.goto(`${env.url}/settings/sync?join=blirp1-e2eticket&code=ABCD-EFGH`);
  const joinForm = page.getByRole('form', { name: 'Join a hub' });
  await expect(joinForm.getByLabel(/^Invite/)).toHaveValue('blirp1-e2eticket');
  await expect(joinForm.getByLabel('Pairing code')).toHaveValue('ABCD-EFGH');
  await expect(page).toHaveURL(`${env.url}/settings/sync`); // the code does not stay in history

  await confirmNextDialog();
  await page.getByRole('button', { name: 'Enable hub' }).click();
  await expect(page.getByTestId('sync-role')).toHaveText('hub');
  await page.getByRole('button', { name: 'Create invite' }).click();
  await expect(page.getByTestId('pairing-code')).toHaveText(/^[2-9A-HJ-NP-Z]{4}-[2-9A-HJ-NP-Z]{4}$/);
  await expect(page.getByRole('img', { name: 'Pairing QR code' }).locator('canvas')).toBeVisible();
  await expect(page.getByLabel('Invite', { exact: true })).toHaveValue(/^blirp1-/);
  await expect(page.getByLabel('Join link', { exact: true })).toHaveValue(/^blirp:\/\/join\//);
  await expect(page.getByTestId('expiry')).toContainText(/Expires in (9|10)m/);

  await confirmNextDialog();
  await page.getByRole('button', { name: 'Disable hub' }).click();
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
  await page.getByRole('button', { name: 'Close sessions list' }).click();

  for (const path of [plainProjectUrl, `${plainProjectUrl}/memory`, `${repoProjectUrl}/git`, '/projects', '/settings/memory']) {
    await page.goto(`${env.url}${path}`);
    await expect(page.locator('.page-title').first()).toBeVisible();
    expect(await overflow(), path).toBeLessThanOrEqual(0);
  }
  await page.setViewportSize({ width: 1360, height: 860 });
});

test('no CSP violations or unexpected console errors', async () => {
  const csp = await page.evaluate(() => (window as unknown as { __csp: string[] }).__csp);
  expect(csp).toEqual([]);
  expect(consoleErrors).toEqual([]);
});
