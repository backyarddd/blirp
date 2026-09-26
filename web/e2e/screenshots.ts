// Regenerates the README screenshots in docs/images/ (`pnpm -C web screenshots`).
//
// Starts the real daemon (`cargo build -p blirp`) on a fresh demo root with HOME, USERPROFILE,
// APPDATA, LOCALAPPDATA, the XDG dirs and every agent data dir pointed inside it, so ingest can
// only see the synthetic transcripts written here, never the real user's. The root is a fixed,
// neutral path (`C:\demo` on Windows, `/tmp/demo` elsewhere; override with BLIRP_SHOTS_ROOT)
// because the UI and the terminal show absolute paths. It must not exist yet; it is removed at
// the end. Seeds projects, memory and sessions through the HTTP API, then captures the SPA
// headless in dark mode.
import { execFileSync, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { createWriteStream, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';
import { chromium, type Page } from '@playwright/test';

const repoRoot = resolve(import.meta.dirname, '../..');
const outDir = join(repoRoot, 'docs', 'images');
const root = process.env.BLIRP_SHOTS_ROOT ?? (process.platform === 'win32' ? 'C:\\demo' : '/tmp/demo');
const PORT = 47791;
const LIVE = ['starting', 'working', 'idle', 'waiting'];

interface Session {
  id: string;
  status: string;
  agent_session_id: string | null;
}
interface Project {
  id: string;
  name: string;
}

async function waitFor<T>(what: string, timeoutMs: number, probe: () => Promise<T | undefined>): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const v = await probe();
      if (v !== undefined) return v;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${String(last)}` : ''}`);
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

function git(cwd: string, ...args: string[]): void {
  execFileSync('git', args, { cwd, stdio: 'pipe' });
}

function write(path: string, text: string): void {
  mkdirSync(resolve(path, '..'), { recursive: true });
  writeFileSync(path, text);
}

/** A git repository with a synthetic author and one commit per entry of `commits`. */
function repo(dir: string, commits: [string, Record<string, string>][]): void {
  mkdirSync(dir, { recursive: true });
  git(dir, 'init', '-q', '-b', 'main');
  git(dir, 'config', 'user.name', 'Demo Developer');
  git(dir, 'config', 'user.email', 'dev@example.com');
  git(dir, 'config', 'commit.gpgsign', 'false');
  for (const [message, files] of commits) {
    for (const [name, text] of Object.entries(files)) write(join(dir, name), text);
    git(dir, 'add', '.');
    git(dir, 'commit', '-q', '-m', message);
  }
}

function seedDisk(code: string): void {
  repo(join(code, 'acme-api'), [
    [
      'feat: orders service skeleton',
      {
        'package.json': JSON.stringify({ name: 'acme-api', private: true, type: 'module', scripts: { test: 'node --test' } }, null, 2) + '\n',
        'README.md': '# acme-api\n\nOrders and billing API.\n',
        'src/orders.js':
          'export function paginate(items, { cursor = null, limit = 20 } = {}) {\n' +
          '  const start = cursor === null ? 0 : items.findIndex((o) => o.id === cursor) + 1;\n' +
          '  const page = items.slice(start, start + limit);\n' +
          '  const next = start + limit < items.length ? page.at(-1).id : null;\n' +
          '  return { items: page, next_cursor: next };\n' +
          '}\n\n' +
          'export function total(order) {\n' +
          '  return order.lines.reduce((sum, l) => sum + l.qty * l.unit_cents, 0);\n' +
          '}\n',
      },
    ],
    [
      'test: cover pagination and totals',
      {
        'test/orders.test.js':
          "import { test, describe } from 'node:test';\n" +
          "import assert from 'node:assert/strict';\n" +
          "import { paginate, total } from '../src/orders.js';\n\n" +
          'const orders = Array.from({ length: 45 }, (_, i) => ({ id: `ord_${i + 1}` }));\n\n' +
          "describe('paginate', () => {\n" +
          "  test('first page has the default size', () => assert.equal(paginate(orders).items.length, 20));\n" +
          "  test('cursor continues after the given id', () => {\n" +
          "    assert.equal(paginate(orders, { cursor: 'ord_20' }).items[0].id, 'ord_21');\n" +
          '  });\n' +
          "  test('last page has no next cursor', () => {\n" +
          "    assert.equal(paginate(orders, { cursor: 'ord_40' }).next_cursor, null);\n" +
          '  });\n' +
          '});\n\n' +
          "describe('total', () => {\n" +
          "  test('sums quantity times unit price', () => {\n" +
          '    assert.equal(total({ lines: [{ qty: 2, unit_cents: 1250 }, { qty: 1, unit_cents: 499 }] }), 2999);\n' +
          '  });\n' +
          "  test('an empty order is zero', () => assert.equal(total({ lines: [] }), 0));\n" +
          '});\n',
        'test/webhooks.test.js':
          "import { test } from 'node:test';\n" +
          "import assert from 'node:assert/strict';\n\n" +
          "test('retries a failed delivery with backoff', async () => {\n" +
          '  const delays = [1, 2, 4].map((n) => n * 100);\n' +
          '  assert.deepEqual(delays, [100, 200, 400]);\n' +
          '});\n\n' +
          "test('drops events older than 24 h', () => assert.ok(true));\n",
      },
    ],
    ['feat(orders): cursor pagination for GET /orders', { 'CHANGELOG.md': '## Unreleased\n\n- Cursor pagination on `GET /orders`.\n' }],
  ]);
  // One uncommitted change, so the git tab has something to show.
  write(join(code, 'acme-api', 'README.md'), '# acme-api\n\nOrders and billing API.\n\nRun `npm test` before pushing.\n');

  repo(join(code, 'infra'), [
    ['chore: bootstrap staging environment', { 'staging/main.tf': 'module "network" {\n  source = "../modules/network"\n  cidr   = "10.20.0.0/16"\n}\n', 'README.md': '# infra\n' }],
    ['feat(network): private subnets for the database tier', { 'modules/network/subnets.tf': '# private subnets\n' }],
    ['fix(dns): lower TTL before the cutover', { 'staging/dns.tf': 'ttl = 300\n' }],
  ]);

  const notes = join(code, 'design-notes');
  write(join(notes, 'checkout-flow.md'), '# Checkout flow\n\nOne page, address first, payment last.\n');
  write(join(notes, 'onboarding.md'), '# Onboarding\n\nThree steps, skippable after the first.\n');
}

/** A finished Claude Code transcript, as ingest finds it under CLAUDE_CONFIG_DIR. */
function seedClaudeTranscript(userHome: string, cwd: string): string {
  const sid = randomUUID();
  const start = Date.now() - 3 * 60 * 60_000;
  const common = { cwd, sessionId: sid, version: '2.1.0', userType: 'external', entrypoint: 'cli', gitBranch: 'main' };
  const usage = { input_tokens: 1840, cache_creation_input_tokens: 5200, cache_read_input_tokens: 38000, output_tokens: 920 };
  const turns: [string, string][] = [
    ['user', 'GET /orders returns everything at once. Add cursor pagination with a default page size of 20.'],
    ['assistant', 'I added `paginate()` in src/orders.js: it takes `cursor` and `limit`, returns `items` and `next_cursor`, and the route passes the query parameters through.'],
    ['user', 'Add tests for the first page, a middle page and the last page.'],
    ['assistant', 'Added three cases to test/orders.test.js. All 7 tests pass with `npm test`.'],
  ];
  const lines = turns.map(([role, text], i) =>
    JSON.stringify({
      parentUuid: i === 0 ? null : `t${i - 1}`,
      isSidechain: false,
      type: role,
      message:
        role === 'user'
          ? { role, content: text }
          : { model: 'claude-sonnet-4-5', id: `msg_${i}`, type: 'message', role, content: [{ type: 'text', text }], usage },
      uuid: `t${i}`,
      timestamp: new Date(start + i * 90_000).toISOString(),
      ...common,
    }),
  );
  write(join(userHome, '.claude', 'projects', 'acme-api', `${sid}.jsonl`), lines.join('\n') + '\n');
  return sid;
}

const BRIEF = `acme-api is the orders and billing HTTP API behind the storefront. Node 22, ES modules, no framework; tests use \`node:test\`.

**Build and run**
- \`npm test\` runs the whole suite (about 2 s).
- \`npm start\` serves on port 8080; set \`DATABASE_URL\` first.

**Current priorities**
1. Cursor pagination on every list endpoint (\`/orders\` done, \`/invoices\` next).
2. Make webhook delivery idempotent before the billing split.
`;

const RECORDS = [
  { kind: 'decision', title: 'Cursor pagination, not offsets', body: 'Offsets skip rows when orders are inserted between pages. Cursors are the last id of the page; `next_cursor` is null on the last page.', pinned: true },
  { kind: 'decision', title: 'Validate request bodies at the route boundary', body: 'Handlers receive parsed, typed input only. Invalid input is a 400 with the field path, never a 500.' },
  { kind: 'open_thread', title: 'Webhook retries can deliver twice', body: 'A timeout after the receiver committed triggers a retry. Needs an idempotency key per event before the billing split.' },
  { kind: 'open_thread', title: 'Paginate /invoices', body: 'Same shape as /orders. The mobile client already sends `cursor`.' },
  { kind: 'gotcha', title: 'Money is integer cents', body: 'Never use floats for amounts. `total()` returns cents; format only at the edge.' },
  { kind: 'gotcha', title: 'Tests assume TZ=UTC', body: 'Date bucketing tests fail in other time zones; CI sets TZ=UTC.' },
];

const WIKI = [
  {
    slug: 'local-setup',
    title: 'Local setup',
    body_md: '## Requirements\n\n- Node 22\n- PostgreSQL 16 (Docker is fine)\n\n## First run\n\n```sh\nnpm install\ncp .env.example .env\nnpm test\n```\n',
  },
  {
    slug: 'release-checklist',
    title: 'Release checklist',
    body_md: '1. `npm test` is green on main.\n2. CHANGELOG has an entry for every user-visible change.\n3. Tag `vX.Y.Z` and let CI publish.\n',
  },
];

async function main(): Promise<void> {
  if (existsSync(root)) throw new Error(`${root} already exists; remove it or set BLIRP_SHOTS_ROOT to a path that does not`);
  if (!existsSync(join(repoRoot, 'web', 'dist', 'index.html'))) throw new Error('web/dist is missing; run `pnpm -C web build` first');
  execFileSync('cargo', ['build', '-p', 'blirp'], { cwd: repoRoot, stdio: 'inherit' });
  const bin = join(process.env.CARGO_TARGET_DIR ?? join(repoRoot, 'target'), 'debug', process.platform === 'win32' ? 'blirp.exe' : 'blirp');

  // The demo root doubles as the user's home: the terminal prompt and project paths stay short.
  const userHome = root;
  const blirpHome = join(root, '.blirp');
  const code = join(root, 'code');
  mkdirSync(blirpHome, { recursive: true });
  mkdirSync(join(userHome, '.claude', 'projects'), { recursive: true });
  writeFileSync(
    join(blirpHome, 'config.toml'),
    '[machine]\nname = "workstation"\n\n[memory]\nsummarizer = "none"\n\n[sync]\nrelay = "disabled"\n\n[update]\ncheck = false\n',
  );
  seedDisk(code);

  // Agents installed under the real home (npm globals, ~/.local/bin, ...) stay out of PATH, so
  // no personal install path can appear in the UI.
  const realHome = homedir().toLowerCase();
  const path = (process.env.PATH ?? process.env.Path ?? '')
    .split(delimiter)
    .filter((p) => p && !p.toLowerCase().startsWith(realHome))
    .join(delimiter);
  const { CLAUDE_CODE_OAUTH_TOKEN: _token, Path: _winPath, ...parentEnv } = process.env;
  const log = createWriteStream(join(root, 'daemon.log'));
  const daemon = spawn(bin, ['daemon', '--port', String(PORT)], {
    cwd: code,
    windowsHide: true,
    env: {
      ...parentEnv,
      PATH: path,
      BLIRP_HOME: blirpHome,
      BLIRP_LOOPBACK_ONLY: '1',
      HOME: userHome,
      USERPROFILE: userHome,
      HOMEDRIVE: '',
      HOMEPATH: '',
      APPDATA: join(userHome, 'AppData', 'Roaming'),
      LOCALAPPDATA: join(userHome, 'AppData', 'Local'),
      XDG_CONFIG_HOME: join(userHome, '.config'),
      XDG_DATA_HOME: join(userHome, '.local', 'share'),
      XDG_STATE_HOME: join(userHome, '.local', 'state'),
      XDG_CACHE_HOME: join(userHome, '.cache'),
      CLAUDE_CONFIG_DIR: join(userHome, '.claude'),
      CODEX_HOME: join(userHome, '.codex'),
      GEMINI_CLI_HOME: userHome,
      CURSOR_CONFIG_DIR: join(userHome, '.cursor'),
      AMP_DATA_DIR: join(userHome, '.local', 'share', 'amp'),
      PI_CODING_AGENT_DIR: join(userHome, '.pi', 'agent'),
      DSH_HOME: join(userHome, '.dsh'),
      RUST_LOG: 'info',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  daemon.stdout.pipe(log);
  daemon.stderr.pipe(log);
  let exited = false;
  daemon.on('exit', () => (exited = true));

  const browser = await chromium.launch({ channel: process.env.BLIRP_E2E_CHANNEL ?? 'msedge', headless: true });
  try {
    const { token } = await waitFor('runtime.json', 60_000, async () => {
      if (exited) throw new Error(`daemon exited early; see ${join(root, 'daemon.log')}`);
      const file = join(blirpHome, 'runtime.json');
      return existsSync(file) ? (JSON.parse(readFileSync(file, 'utf8')) as { token: string }) : undefined;
    });
    const url = `http://127.0.0.1:${PORT}`;
    const api = async <T>(method: string, p: string, body?: unknown): Promise<T> => {
      const res = await fetch(`${url}${p}`, {
        method,
        headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      const text = await res.text();
      if (!res.ok) throw new Error(`${method} ${p}: ${res.status} ${text}`);
      return (text ? JSON.parse(text) : undefined) as T;
    };
    await waitFor('/api/health', 30_000, async () => ((await fetch(`${url}/api/health`, { headers: { Authorization: `Bearer ${token}` } })).ok ? true : undefined));

    const project = async (dir: string): Promise<string> => (await api<Project>('POST', '/api/projects', { path: join(code, dir) })).id;
    const acme = await project('acme-api');
    const infra = await project('infra');
    const notes = await project('design-notes');

    await api('PUT', `/api/projects/${acme}/brief`, { body_md: BRIEF });
    for (const r of RECORDS) await api('POST', `/api/projects/${acme}/records`, r);
    for (const w of WIKI) await api('POST', `/api/projects/${acme}/wiki`, w);
    await api('POST', `/api/projects/${acme}/resources`, { kind: 'doc', url: 'https://example.com/acme/api-guidelines', title: 'API guidelines' });
    await api('PUT', `/api/projects/${infra}/brief`, { body_md: 'Terraform for the staging and production environments. `terraform plan` in `staging/` before every apply.\n' });
    await api('POST', `/api/projects/${infra}/records`, { kind: 'decision', title: 'One state file per environment', body: 'Staging and production never share state; modules are shared.' });

    // Written after the projects exist, so ingest files it under acme-api instead of creating it.
    const claudeSid = seedClaudeTranscript(userHome, join(code, 'acme-api'));
    const claude = await waitFor('the demo Claude Code session to be ingested', 60_000, async () =>
      (await api<{ items: Session[] }>('GET', '/api/sessions?limit=200')).items.find((s) => s.agent_session_id === claudeSid),
    );
    await api('PATCH', `/api/sessions/${claude.id}`, { title: 'Cursor pagination for GET /orders' });

    const launch = async (projectId: string, title: string): Promise<Session> => {
      const s = await api<Session>('POST', '/api/sessions', { project_id: projectId, agent: 'shell', cols: 120, rows: 32 });
      await api('PATCH', `/api/sessions/${s.id}`, { title });
      await waitFor(`${title} to be idle`, 30_000, async () => ((await api<Session>('GET', `/api/sessions/${s.id}`)).status === 'idle' ? true : undefined));
      return s;
    };
    const ended = await launch(notes, 'Outline the checkout copy');
    await api('POST', `/api/sessions/${ended.id}/stop`);
    await waitFor('the notes session to end', 30_000, async () => (LIVE.includes((await api<Session>('GET', `/api/sessions/${ended.id}`)).status) ? undefined : true));
    await launch(infra, 'Review staging network changes');
    const tests = await launch(acme, 'Run the test suite');

    // Only what this script seeded may exist: proves nothing from the real home was ingested.
    const all = await api<{ items: Session[] }>('GET', '/api/sessions?limit=200&include_children=true');
    const projects = await api<Project[]>('GET', '/api/projects');
    if (all.items.length !== 4) throw new Error(`expected 4 demo sessions, found ${all.items.length}`);
    const names = projects.map((p) => p.name).sort();
    if (names.join() !== 'acme-api,design-notes,infra') throw new Error(`unexpected projects: ${names.join(', ')}`);

    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark', deviceScaleFactor: 1 });
    const page = await context.newPage();
    await page.goto(`${url}/sessions/${tests.id}#token=${token}`);
    await page.locator('.xterm').waitFor();

    const type = async (p: Page, command: string): Promise<void> => {
      await p.locator('.xterm').click();
      await p.keyboard.type(command);
      await p.keyboard.press('Enter');
    };
    await sleep(1500);
    await type(page, 'clear');
    await sleep(800);
    await type(page, 'npm test');
    // xterm draws on a canvas, so read the daemon's screen snapshot instead of the DOM.
    const screen = async (id: string): Promise<string> => {
      const wsPath = `/api/terminals/${id}/ws`;
      const { ticket } = await api<{ ticket: string }>('POST', '/api/ws-ticket', { path: wsPath });
      return new Promise<string>((ok, fail) => {
        const ws = new WebSocket(`ws://127.0.0.1:${PORT}${wsPath}?ticket=${ticket}`);
        ws.onmessage = (ev) => {
          if (typeof ev.data !== 'string') return;
          const m = JSON.parse(ev.data) as { type: string; data?: string };
          if (m.type !== 'snapshot') return;
          ws.close();
          ok(m.data ?? '');
        };
        ws.onerror = () => fail(new Error('terminal websocket failed'));
      });
    };
    await waitFor('the test run to finish', 60_000, async () => ((await screen(tests.id)).includes('duration_ms') ? true : undefined));
    await sleep(1000);

    mkdirSync(outDir, { recursive: true });
    const shot = async (name: string): Promise<void> => {
      await sleep(600);
      await page.screenshot({ path: join(outDir, `${name}.png`) });
      console.log(`wrote docs/images/${name}.png`);
    };

    // 1. Workspace: live terminal with the memory panel showing what the session was given.
    await page.goto(`${url}/sessions/${tests.id}`);
    await page.locator('.xterm').waitFor();
    const toggle = page.getByRole('button', { name: 'Memory panel', exact: true });
    if ((await toggle.getAttribute('aria-pressed')) !== 'true') await toggle.click();
    await shot('workspace');

    // 3. Project memory: brief and records.
    await page.goto(`${url}/projects/${acme}/memory`);
    // Taller, so the records fit below the brief (the page scrolls inside its own container).
    await page.setViewportSize({ width: 1440, height: 1480 });
    await page.getByText('Cursor pagination, not offsets').first().waitFor();
    await shot('memory');
    await page.setViewportSize({ width: 1440, height: 900 });

    // 4. New session dialog.
    await page.goto(`${url}/sessions/${tests.id}`);
    await page.locator('.xterm').waitFor();
    await page.getByRole('button', { name: 'New session' }).first().click();
    const dialog = page.getByRole('dialog', { name: 'New session' });
    await dialog.waitFor();
    await dialog.getByLabel('Project').selectOption({ label: 'acme-api' });
    await dialog.getByPlaceholder('What should the agent work on?').fill('Paginate /invoices the same way as /orders, with tests.');
    await sleep(1500);
    await shot('new-session');
    await page.keyboard.press('Escape');

    // 5. Machines & Sync with this machine as the hub and a pairing invite.
    await page.goto(`${url}/settings/sync`);
    page.once('dialog', (d) => void d.accept());
    await page.getByRole('button', { name: 'Enable hub' }).click();
    await page.getByTestId('sync-role').filter({ hasText: 'hub' }).waitFor();
    await page.getByRole('button', { name: 'Create invite' }).click();
    await page.getByText('Pairing code').waitFor();
    for (const b of await page.locator('.toast').getByRole('button', { name: 'Dismiss' }).all()) await b.click();
    await shot('sync');

    // 6. Phone width.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`${url}/projects/${acme}`);
    await page.locator('.page-title').first().waitFor();
    await shot('mobile');
    await context.close();
  } finally {
    await browser.close();
    if (!exited) {
      const done = new Promise((r) => daemon.once('exit', r));
      daemon.kill();
      await done;
    }
    await new Promise<void>((r) => log.end(r));
    if (process.env.BLIRP_SHOTS_KEEP) console.log(`kept ${root}`);
    else rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 });
  }
}

await main();
