// Session and project actions: context menus (never over a terminal), F2 rename, archive, bulk
// move with a partial failure, and a project's round trip through the Trash. Runs against the same
// daemon as the other specs, in its own folders and projects. Tests share one page and run in order.
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Locator, type Page } from '@playwright/test';
import { e2eEnv } from './env';

const env = e2eEnv();
const AUTH = { Authorization: `Bearer ${env.token}` };
const LIVE = ['starting', 'working', 'idle', 'waiting'];

test.describe.configure({ mode: 'serial' });

let page: Page;

interface SessionRow {
  id: string;
  status: string;
  title: string | null;
  project_id: string;
}

interface ProjectRow {
  id: string;
  name: string;
  deleted: boolean;
  paths: { path: string }[];
}

async function apiCall<T>(method: 'GET' | 'POST' | 'DELETE' | 'PATCH', path: string, data?: unknown): Promise<T> {
  const res = await page.request.fetch(`${env.url}${path}`, { method, data, headers: { Origin: env.url, ...AUTH } });
  const text = await res.text();
  expect(res.ok(), `${method} ${path}: ${res.status()} ${text}`).toBe(true);
  return (text ? JSON.parse(text) : undefined) as T;
}

/** A project for one test, in a folder of its own. */
async function project(name: string): Promise<ProjectRow> {
  const dir = join(env.root, 'ui-actions', name);
  mkdirSync(dir, { recursive: true });
  return apiCall<ProjectRow>('POST', '/api/projects', { path: dir, name });
}

async function endedShell(projectId: string, title: string): Promise<SessionRow> {
  const s = await apiCall<SessionRow>('POST', '/api/sessions', { agent: 'shell', project_id: projectId });
  await expect.poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status, { timeout: 30_000 }).toBe('idle');
  await apiCall('POST', `/api/sessions/${s.id}/stop`);
  await expect
    .poll(async () => LIVE.includes((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status), { timeout: 30_000 })
    .toBe(false);
  return apiCall<SessionRow>('PATCH', `/api/sessions/${s.id}`, { title });
}

const sidebar = (): Locator => page.getByRole('complementary', { name: 'Sessions' });
const card = (id: string): Locator => sidebar().locator(`a[href="/sessions/${id}"]`);
/** The open context menu (the host's menu is named after the item). */
const openMenu = (): Locator => page.locator('.menu:popover-open');

test.beforeAll(async ({ browser }) => {
  const context = await browser.newContext();
  page = await context.newPage();
  await page.goto(`${env.url}/sessions#token=${env.token}`);
  await expect(page).toHaveURL(`${env.url}/sessions`);
});

test.afterAll(async () => {
  await page?.context().close();
});

test('right-click on a session card opens its menu; Rename renames it, Undo takes it back', async () => {
  const p = await project('Menu project');
  const s = await endedShell(p.id, 'Before rename');
  await page.goto(`${env.url}/sessions/${s.id}`);
  await card(s.id).click({ button: 'right' });
  const menu = page.getByRole('menu', { name: 'Before rename', exact: true });
  await expect(menu).toBeVisible();
  // Keyboard: the first item has focus, arrows move it.
  await expect(menu.getByRole('menuitem').first()).toBeFocused();
  await menu.getByRole('menuitem', { name: 'Rename…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Rename session' });
  await dialog.getByLabel('Session title').fill('Renamed from the menu');
  await dialog.getByRole('button', { name: 'Rename' }).click();
  await expect(dialog).toBeHidden();
  await expect(card(s.id)).toContainText('Renamed from the menu');
  await expect.poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).title).toBe('Renamed from the menu');

  const toast = page.locator('.toast', { hasText: 'Renamed to "Renamed from the menu"' });
  await toast.getByRole('button', { name: 'Undo' }).click();
  await expect(card(s.id)).toContainText('Before rename');

  // The "⋯" button opens the same actions without a mouse's right button.
  await card(s.id).hover();
  await sidebar().getByRole('button', { name: 'Actions for Before rename' }).click();
  await expect(page.getByRole('menu', { name: 'Actions for Before rename', exact: true }).getByRole('menuitem', { name: 'Copy session id' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(openMenu()).toHaveCount(0);
});

test('F2 on a focused session card renames it; Shift+F10 opens its menu', async () => {
  const p = await apiCall<ProjectRow[]>('GET', '/api/projects').then((all) => all.find((x) => x.name === 'Menu project'));
  if (!p) throw new Error('Menu project is missing');
  const s = await endedShell(p.id, 'Keyboard title');
  await page.goto(`${env.url}/sessions/${s.id}`);
  await card(s.id).focus();
  await page.keyboard.press('F2');
  const dialog = page.getByRole('dialog', { name: 'Rename session' });
  await expect(dialog.getByLabel('Session title')).toBeFocused();
  await dialog.getByLabel('Session title').fill('Renamed with F2');
  await page.keyboard.press('Enter');
  await expect(dialog).toBeHidden();
  await expect(card(s.id)).toContainText('Renamed with F2');

  await card(s.id).focus();
  await page.keyboard.press('Shift+F10');
  const menu = page.getByRole('menu', { name: 'Renamed with F2', exact: true });
  await expect(menu).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(menu).toBeHidden();
  // Focus goes back to the card the menu was opened on.
  await expect(card(s.id)).toBeFocused();
});

test('the context menu never opens over a terminal, in the session pane or the grid', async () => {
  const p = await apiCall<ProjectRow[]>('GET', '/api/projects').then((all) => all.find((x) => x.name === 'Menu project'));
  if (!p) throw new Error('Menu project is missing');
  const s = await apiCall<SessionRow>('POST', '/api/sessions', { agent: 'shell', project_id: p.id });
  try {
    await page.goto(`${env.url}/sessions/${s.id}`);
    const term = page.locator('.frame .xterm');
    await term.waitFor();
    await term.click({ button: 'right' });
    await page.waitForTimeout(300);
    await expect(openMenu()).toHaveCount(0);

    await page.goto(`${env.url}/grid`);
    const tile = page.locator('section.tile').filter({ has: page.locator(`a[href="/sessions/${s.id}"]`) });
    const header = tile.locator('header');
    await tile.locator('.xterm').click({ button: 'right' });
    await page.waitForTimeout(300);
    await expect(openMenu()).toHaveCount(0);
    // Its header has the session's menu.
    await header.click({ button: 'right' });
    await expect(openMenu()).toHaveCount(1);
    await expect(openMenu().getByRole('menuitem', { name: 'Stop', exact: true })).toBeVisible();
    await page.keyboard.press('Escape');
  } finally {
    await apiCall('POST', `/api/sessions/${s.id}/stop`);
  }
});

test('archive hides a session until Show archived; Unarchive brings it back', async () => {
  const p = await project('Archive project');
  const s = await endedShell(p.id, 'To be archived');
  await page.goto(`${env.url}/sessions`);
  await expect(card(s.id)).toBeVisible();
  await card(s.id).click({ button: 'right' });
  await page.getByRole('menu', { name: 'To be archived', exact: true }).getByRole('menuitem', { name: 'Archive' }).click();
  await expect(card(s.id)).toHaveCount(0);
  await expect(page.locator('.toast', { hasText: 'Archived "To be archived"' })).toBeVisible();

  const show = sidebar().getByLabel('Show archived');
  await show.check();
  await expect(card(s.id)).toBeVisible();
  await expect(card(s.id)).toContainText('archived');
  // Per device: nothing changed on the daemon.
  expect((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).title).toBe('To be archived');
  await card(s.id).click({ button: 'right' });
  await page.getByRole('menu', { name: 'To be archived', exact: true }).getByRole('menuitem', { name: 'Unarchive' }).click();
  await show.uncheck();
  await expect(card(s.id)).toBeVisible();
  // Survives a reload (stored per browser).
  await card(s.id).click({ button: 'right' });
  await page.getByRole('menu', { name: 'To be archived', exact: true }).getByRole('menuitem', { name: 'Archive' }).click();
  await page.reload();
  await expect(sidebar().getByRole('region', { name: 'Menu project' })).toBeVisible();
  await expect(card(s.id)).toHaveCount(0);
  await sidebar().getByLabel('Show archived').check();
  await card(s.id).click({ button: 'right' });
  await page.getByRole('menu', { name: 'To be archived', exact: true }).getByRole('menuitem', { name: 'Unarchive' }).click();
  await sidebar().getByLabel('Show archived').uncheck();
});

test('bulk move: shift-click selects a range; one toast names the session that could not move', async () => {
  const from = await project('Bulk from');
  const to = await project('Bulk target');
  const a = await endedShell(from.id, 'Bulk one');
  const b = await endedShell(from.id, 'Bulk two');
  const c = await endedShell(from.id, 'Bulk three');
  // One move fails on the way, as when the session's machine went offline.
  await page.route(`**/api/sessions/${b.id}/move`, (route) =>
    route.fulfill({
      status: 503,
      contentType: 'application/json',
      body: JSON.stringify({ error: { code: 'proxy_failed', message: 'the laptop did not answer' } }),
    }),
  );
  await page.goto(`${env.url}/sessions`);
  const group = sidebar().getByRole('region', { name: 'Bulk from' });
  await expect(group.locator('a.scard')).toHaveCount(3);
  await sidebar().getByRole('button', { name: 'Select', exact: true }).click();
  const boxes = group.getByRole('checkbox');
  await boxes.first().click();
  await boxes.last().click({ modifiers: ['Shift'] });
  const bar = sidebar().getByRole('toolbar', { name: 'Selected sessions' });
  await expect(bar).toContainText('3 selected');
  await bar.getByRole('button', { name: 'Move selected' }).click();
  const dialog = page.getByRole('dialog', { name: 'Move sessions' });
  await dialog.getByRole('combobox').selectOption({ label: 'Bulk target' });
  await dialog.getByRole('button', { name: 'Move' }).click();
  const toast = page.locator('.toast', { hasText: 'Moved 2 of 3 sessions to "Bulk target"' });
  await expect(toast).toContainText('Not moved: "Bulk two" (the laptop did not answer)');
  await page.unroute(`**/api/sessions/${b.id}/move`);
  for (const [s, where] of [
    [a, to.id],
    [b, from.id],
    [c, to.id],
  ] as const) {
    expect((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).project_id).toBe(where);
  }
  // Undo moves back what moved.
  await toast.getByRole('button', { name: 'Undo' }).click();
  await expect.poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${a.id}`)).project_id).toBe(from.id);
  expect((await apiCall<SessionRow>('GET', `/api/sessions/${c.id}`)).project_id).toBe(from.id);
  await bar.getByRole('button', { name: 'Done' }).click();
  await expect(bar).toHaveCount(0);
});

test('deleting a project moves it to the Trash with its sessions; Restore brings both back', async () => {
  const p = await project('Trash project');
  const s = await endedShell(p.id, 'Trash session');
  await page.goto(`${env.url}/projects/${p.id}`);
  await page.getByRole('button', { name: 'Project actions' }).click();
  await page.getByRole('menuitem', { name: 'Delete…', exact: true }).click();
  const confirm = page.getByRole('dialog', { name: 'Delete project?' });
  await expect(confirm).toContainText('folders on other machines are not restored');
  await confirm.getByRole('button', { name: 'Move to Trash' }).click();
  await expect(page).toHaveURL(`${env.url}/projects`);
  await expect(page.locator('.pcard', { hasText: 'Trash project' })).toHaveCount(0);

  // Its session is gone from the lists, not shown under an "Unknown project".
  await page.goto(`${env.url}/sessions`);
  await expect(sidebar().getByRole('region', { name: 'Menu project' })).toBeVisible();
  await expect(card(s.id)).toHaveCount(0);
  await expect(sidebar()).not.toContainText('Unknown project');

  await page.goto(`${env.url}/projects`);
  await page.getByRole('link', { name: 'Trash' }).click();
  await expect(page).toHaveURL(`${env.url}/trash`);
  const row = page.locator('.rows li', { hasText: 'Trash project' });
  await expect(row).toContainText('1 session');
  await row.getByRole('button', { name: 'Restore' }).click();
  await expect(page.locator('.toast', { hasText: 'Restored "Trash project"' })).toBeVisible();
  await expect(row).toHaveCount(0);

  const back = await apiCall<ProjectRow>('GET', `/api/projects/${p.id}`);
  expect(back.deleted).toBe(false);
  // This machine's folder is registered again.
  expect(back.paths).toHaveLength(1);
  await page.goto(`${env.url}/sessions`);
  await expect(sidebar().getByRole('region', { name: 'Trash project' }).locator(`a[href="/sessions/${s.id}"]`)).toBeVisible();
});

test('Stop and delete stops a running session, then deletes it', async () => {
  const p = await project('Stop delete project');
  const s = await apiCall<SessionRow>('POST', '/api/sessions', { agent: 'shell', project_id: p.id });
  await expect.poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status, { timeout: 30_000 }).toBe('idle');
  await page.goto(`${env.url}/sessions/${s.id}`);
  await card(s.id).click({ button: 'right' });
  await openMenu().getByRole('menuitem', { name: 'Stop and delete…' }).click();
  await page.getByRole('dialog', { name: 'Stop and delete session?' }).getByRole('button', { name: 'Stop and delete' }).click();
  await expect(page.locator('.toast', { hasText: 'Session stopped and deleted' })).toBeVisible({ timeout: 35_000 });
  expect((await page.request.get(`${env.url}/api/sessions/${s.id}`, { headers: AUTH })).status()).toBe(404);
});

test('the Trash reads again when a project is deleted, not when one is renamed', async () => {
  const p = await project('Trash reload');
  await page.goto(`${env.url}/projects`);
  await expect(page.locator('.pcard', { hasText: 'Trash reload' })).toBeVisible();
  let reads = 0;
  const count = (r: { url(): string }): void => {
    if (r.url().includes('/api/projects?deleted=true')) reads += 1;
  };
  page.on('request', count);
  try {
    await page.getByRole('link', { name: 'Trash', exact: true }).click();
    await expect.poll(() => reads).toBe(1);
    await apiCall('PATCH', `/api/projects/${p.id}`, { name: 'Trash reload renamed' });
    // Time for the pushed update to arrive; it must not read the Trash again.
    await page.waitForTimeout(1500);
    expect(reads).toBe(1);
    await apiCall('DELETE', `/api/projects/${p.id}`);
    // The Trash view reads it again; the app also asks the Trash whether the project was merged away.
    await expect.poll(() => reads).toBeGreaterThanOrEqual(2);
    await expect(page.locator('.rows li', { hasText: 'Trash reload renamed' })).toBeVisible();
  } finally {
    page.off('request', count);
  }
});

test('Undo leaves an item alone that was changed again since', async () => {
  const p = await project('Undo guard');
  const s = await endedShell(p.id, 'First title');
  await page.goto(`${env.url}/sessions/${s.id}`);
  await card(s.id).click({ button: 'right' });
  await openMenu().getByRole('menuitem', { name: 'Rename…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Rename session' });
  await dialog.getByLabel('Session title').fill('Second title');
  await dialog.getByRole('button', { name: 'Rename' }).click();
  const toast = page.locator('.toast', { hasText: 'Renamed to "Second title"' });
  await expect(toast).toBeVisible();
  // Another client renames it meanwhile.
  await apiCall('PATCH', `/api/sessions/${s.id}`, { title: 'Third title' });
  await expect(card(s.id)).toContainText('Third title');
  await toast.getByRole('button', { name: 'Undo' }).click();
  await expect(page.locator('.toast', { hasText: 'Not undone: it was changed again since.' })).toBeVisible();
  await page.waitForTimeout(500);
  expect((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).title).toBe('Third title');
});

test('a merged project loses its pin on this device; a deleted one keeps it for a restore', async () => {
  const from = await project('Pinned merged');
  const into = await project('Merge target');
  const trashed = await project('Pinned trashed');
  const pinned = async (): Promise<string[]> =>
    page.evaluate(() => JSON.parse(localStorage.getItem('blirp.pinned') ?? '[]') as string[]);
  for (const p of [from, trashed]) {
    await page.goto(`${env.url}/projects/${p.id}`);
    await page.getByRole('button', { name: 'Project actions' }).click();
    await page.getByRole('menuitem', { name: 'Pin', exact: true }).click();
  }
  expect(await pinned()).toEqual(expect.arrayContaining([`p:${from.id}`, `p:${trashed.id}`]));

  await page.goto(`${env.url}/projects/${from.id}`);
  await page.getByRole('button', { name: 'Project actions' }).click();
  await page.getByRole('menuitem', { name: 'Merge into…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Merge project' });
  await dialog.getByRole('combobox').selectOption({ label: 'Merge target' });
  await dialog.getByRole('button', { name: 'Merge' }).click();
  await expect(page).toHaveURL(`${env.url}/projects/${into.id}`);
  await expect.poll(pinned).not.toContain(`p:${from.id}`);

  // Merged by another client: the pushed update drops the pin too.
  const remote = await project('Pinned merged elsewhere');
  await page.goto(`${env.url}/projects/${remote.id}`);
  await page.getByRole('button', { name: 'Project actions' }).click();
  await page.getByRole('menuitem', { name: 'Pin', exact: true }).click();
  await expect.poll(pinned).toContain(`p:${remote.id}`);
  await page.goto(`${env.url}/projects`);
  await apiCall('POST', `/api/projects/${remote.id}/merge`, { into: into.id });
  await expect.poll(pinned).not.toContain(`p:${remote.id}`);

  // Deleted by another client: the pin stays (a restore brings it back).
  await apiCall('DELETE', `/api/projects/${trashed.id}`);
  await expect(page.locator('.toast', { hasText: 'Pinned trashed' })).toHaveCount(0);
  await page.waitForTimeout(1000);
  expect(await pinned()).toContain(`p:${trashed.id}`);
});

interface RecordRow {
  id: string;
  project_id: string;
  title: string;
  status: string;
}

test('records: archive with Undo, move from the context menu, bulk archive and move', async () => {
  const a = await apiCall<ProjectRow>('POST', '/api/projects', { name: 'Records from' });
  const b = await apiCall<ProjectRow>('POST', '/api/projects', { name: 'Records to' });
  const mk = (title: string): Promise<RecordRow> => apiCall<RecordRow>('POST', `/api/projects/${a.id}/records`, { kind: 'note', title, body: '' });
  const r1 = await mk('Record one');
  const r2 = await mk('Record two');
  const r3 = await mk('Record three');
  const get = async (r: RecordRow): Promise<RecordRow | undefined> => {
    for (const pid of [a.id, b.id]) {
      const list = await apiCall<RecordRow[]>('GET', `/api/projects/${pid}/records`);
      const hit = list.find((x) => x.id === r.id);
      if (hit) return hit;
    }
    return undefined;
  };
  await page.goto(`${env.url}/projects/${a.id}/memory`);
  const records = page.locator('section', { has: page.getByRole('heading', { name: 'Records' }) });
  const rec = (title: string): Locator => records.locator('article.rec', { hasText: title });

  await rec('Record one').getByRole('button', { name: 'Archive', exact: true }).click();
  await expect(rec('Record one')).toHaveCount(0);
  await expect.poll(async () => (await get(r1))?.status).toBe('archived');
  await page.locator('.toast', { hasText: 'Archived "Record one"' }).getByRole('button', { name: 'Undo' }).click();
  await expect(rec('Record one')).toBeVisible();
  await expect.poll(async () => (await get(r1))?.status).toBe('active');

  await rec('Record one').locator('.top').click({ button: 'right' });
  await page.getByRole('menu', { name: 'Record one', exact: true }).getByRole('menuitem', { name: 'Move to project…' }).click();
  const move = page.getByRole('dialog', { name: 'Move record' });
  await move.getByRole('combobox').selectOption({ label: 'Records to' });
  await move.getByRole('button', { name: 'Move' }).click();
  await expect(rec('Record one')).toHaveCount(0);
  await expect.poll(async () => (await get(r1))?.project_id).toBe(b.id);

  await records.getByRole('button', { name: 'Select', exact: true }).click();
  const bar = records.getByRole('toolbar', { name: 'Selected records' });
  await rec('Record two').getByRole('checkbox').click();
  await rec('Record three').getByRole('checkbox').click({ modifiers: ['Shift'] });
  await expect(bar).toContainText('2 selected');
  await bar.getByRole('button', { name: 'Archive selected' }).click();
  await expect(page.locator('.toast', { hasText: 'Archived 2 records.' })).toBeVisible();
  for (const r of [r2, r3]) await expect.poll(async () => (await get(r))?.status).toBe('archived');

  await records.getByLabel('Record status').selectOption('archived');
  await bar.getByRole('checkbox').first().check();
  await expect(bar).toContainText('2 selected');
  await bar.getByRole('button', { name: 'Move selected' }).click();
  const moveMany = page.getByRole('dialog', { name: 'Move records' });
  await moveMany.getByRole('combobox').selectOption({ label: 'Records to' });
  await moveMany.getByRole('button', { name: 'Move' }).click();
  await expect(page.locator('.toast', { hasText: 'Moved 2 records to "Records to".' })).toBeVisible();
  for (const r of [r2, r3]) await expect.poll(async () => (await get(r))?.project_id).toBe(b.id);
  await bar.getByRole('button', { name: 'Done' }).click();
});

test('wiki: rename a slug, delete a page with Undo, restore it from Deleted pages', async () => {
  const p = await apiCall<ProjectRow>('POST', '/api/projects', { name: 'Wiki actions' });
  await apiCall('POST', `/api/projects/${p.id}/wiki`, { slug: 'old-name', title: 'Runbook', body_md: 'Restart the thing.' });
  await page.goto(`${env.url}/projects/${p.id}/wiki/old-name`);
  await expect(page.getByRole('heading', { level: 2, name: 'Runbook' })).toBeVisible();

  await page.getByRole('button', { name: 'Edit' }).click();
  await page.getByLabel('Slug').fill('runbook');
  await expect(page.getByText('Links to the old address written in text are not changed.')).toBeVisible();
  await page.getByRole('button', { name: 'Save page' }).click();
  await expect(page).toHaveURL(`${env.url}/projects/${p.id}/wiki/runbook`);
  await expect(page.locator('.content .md')).toContainText('Restart the thing.');

  // A rename whose text then cannot be saved is taken back.
  await page.route(`**/api/projects/${p.id}/wiki/elsewhere`, (route) =>
    route.request().method() === 'PUT'
      ? route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: { code: 'internal', message: 'disk full' } }) })
      : route.fallback(),
  );
  await page.getByRole('button', { name: 'Edit' }).click();
  await page.getByLabel('Slug').fill('elsewhere');
  await page.getByRole('button', { name: 'Save page' }).click();
  await expect(page.getByRole('alert')).toContainText('disk full');
  await page.unroute(`**/api/projects/${p.id}/wiki/elsewhere`);
  await expect
    .poll(async () => (await apiCall<{ slug: string }[]>('GET', `/api/projects/${p.id}/wiki`)).map((w) => w.slug))
    .toEqual(['runbook']);
  await page.getByRole('button', { name: 'Cancel' }).click();

  await page.getByRole('button', { name: 'Delete' }).click();
  await page.getByRole('dialog', { name: 'Delete wiki page?' }).getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page).toHaveURL(`${env.url}/projects/${p.id}/wiki`);
  await expect(page.getByRole('link', { name: 'Runbook' })).toHaveCount(0);
  await page.locator('.toast', { hasText: 'Deleted "Runbook"' }).getByRole('button', { name: 'Undo' }).click();
  await expect(page.getByRole('link', { name: 'Runbook' })).toBeVisible();

  // Deleted again (by another client); restored from the list.
  await apiCall('DELETE', `/api/projects/${p.id}/wiki/runbook`);
  await expect(page.getByRole('link', { name: 'Runbook' })).toHaveCount(0);
  await page.locator('summary', { hasText: 'Deleted pages' }).click();
  const deleted = page.getByRole('list', { name: 'Deleted pages' });
  await deleted.getByRole('button', { name: 'Restore Runbook' }).click();
  await expect(page.locator('.toast', { hasText: 'Restored "Runbook"' }).last()).toBeVisible();
  await expect(page.getByRole('link', { name: 'Runbook' })).toBeVisible();
  await expect(deleted.getByRole('button', { name: 'Restore Runbook' })).toHaveCount(0);
  const live = await apiCall<{ slug: string }[]>('GET', `/api/projects/${p.id}/wiki`);
  expect(live.map((w) => w.slug)).toEqual(['runbook']);
});

test('export: a transcript as Markdown and JSON, a project memory as JSON', async () => {
  const p = await project('Export project');
  const folder = p.paths[0]?.path;
  if (!folder) throw new Error('the project has no folder');
  // A Claude Code transcript in the project's folder, as ingest finds it.
  const sid = randomUUID();
  const start = Date.now() - 30 * 60_000;
  const turn = (uuid: string, parent: string | null, at: number, role: 'user' | 'assistant', text: string): string =>
    JSON.stringify({
      parentUuid: parent,
      isSidechain: false,
      type: role,
      message:
        role === 'user'
          ? { role, content: text }
          : {
              model: 'claude-haiku-4-5',
              id: `msg_${uuid}`,
              type: 'message',
              role,
              content: [{ type: 'text', text }],
              usage: { input_tokens: 1, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 1 },
            },
      uuid,
      timestamp: new Date(start + at).toISOString(),
      cwd: folder,
      sessionId: sid,
      version: '2.0.0',
      userType: 'external',
      entrypoint: 'cli',
    });
  const dir = join(env.userHome, '.claude', 'projects', 'e2e-export');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${sid}.jsonl`), `${turn('u1', null, 1000, 'user', 'Export me please')}\n${turn('a1', 'u1', 2000, 'assistant', 'Here is the ```code``` answer.')}\n`);
  const find = async (): Promise<SessionRow | undefined> =>
    (await apiCall<{ items: (SessionRow & { agent_session_id: string | null })[] }>('GET', `/api/sessions?project=${p.id}&limit=50`)).items.find(
      (x) => x.agent_session_id === sid,
    );
  await expect.poll(find, { timeout: 30_000 }).toBeTruthy();
  const s = await find();
  if (!s) throw new Error('the ingested session disappeared');
  await apiCall('PATCH', `/api/sessions/${s.id}`, { title: 'Export session' });

  await page.goto(`${env.url}/sessions/${s.id}`);
  const saved = async (label: string): Promise<{ name: string; text: string }> => {
    await card(s.id).click({ button: 'right' });
    const [dl] = await Promise.all([page.waitForEvent('download'), openMenu().getByRole('menuitem', { name: label }).click()]);
    const path = await dl.path();
    return { name: dl.suggestedFilename(), text: readFileSync(path, 'utf8') };
  };
  const md = await saved('Export transcript as Markdown');
  expect(md.name).toBe('export-session.md');
  expect(md.text).toContain('# Export session\n');
  expect(md.text).toContain('- Agent: Claude Code\n');
  expect(md.text).toContain('- Project: Export project\n');
  expect(md.text).toContain(`- Folder: ${folder}\n`);
  expect(md.text).toMatch(/## User \([0-9T:.-]+Z\)\n\nExport me please\n/);
  expect(md.text).toContain('Here is the ```code``` answer.');
  const json = await saved('Export transcript as JSON');
  expect(json.name).toBe('export-session.json');
  const parsed = JSON.parse(json.text) as { session: { id: string }; events: { text: string }[] };
  expect(parsed.session.id).toBe(s.id);
  expect(parsed.events.map((e) => e.text)).toEqual(['Export me please', 'Here is the ```code``` answer.']);

  await apiCall('POST', `/api/projects/${p.id}/records`, { kind: 'decision', title: 'Export decision', body: 'Kept.' });
  await page.goto(`${env.url}/projects/${p.id}`);
  await page.getByRole('button', { name: 'Project actions' }).click();
  const [dl] = await Promise.all([page.waitForEvent('download'), page.getByRole('menuitem', { name: 'Export memory as JSON' }).click()]);
  expect(dl.suggestedFilename()).toBe('export-project-memory.json');
  const memory = JSON.parse(readFileSync(await dl.path(), 'utf8')) as { project: { id: string; name: string }; brief: null; records: { title: string }[] };
  expect(Object.keys(memory)).toEqual(['project', 'brief', 'records']);
  expect(memory.project).toMatchObject({ id: p.id, name: 'Export project' });
  expect(memory.records.map((r) => r.title)).toEqual(['Export decision']);
});

test('worktrees page: lists session worktrees with their changes; Prune and a forced Remove', async () => {
  const repo = join(env.root, 'ui-actions', 'wt-repo');
  mkdirSync(repo, { recursive: true });
  const git = (...args: string[]): void => {
    execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', '-c', 'init.defaultBranch=main', ...args], { cwd: repo, stdio: 'pipe' });
  };
  git('init');
  writeFileSync(join(repo, 'a.txt'), 'a\n');
  git('add', '.');
  git('commit', '-m', 'init');
  const p = await apiCall<ProjectRow>('POST', '/api/projects', { path: repo, name: 'Worktree project' });
  const ended = async (title: string): Promise<SessionRow & { worktree: string | null }> => {
    const s = await apiCall<SessionRow>('POST', '/api/sessions', { agent: 'shell', project_id: p.id, worktree: true });
    await expect.poll(async () => (await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status, { timeout: 30_000 }).toBe('idle');
    await apiCall('POST', `/api/sessions/${s.id}/stop`);
    await expect
      .poll(async () => LIVE.includes((await apiCall<SessionRow>('GET', `/api/sessions/${s.id}`)).status), { timeout: 30_000 })
      .toBe(false);
    return apiCall('PATCH', `/api/sessions/${s.id}`, { title });
  };
  const clean = await ended('Clean worktree');
  const dirty = await ended('Dirty worktree');
  if (!clean.worktree || !dirty.worktree) throw new Error('a session got no worktree');
  writeFileSync(join(dirty.worktree, 'scratch.txt'), 'uncommitted\n');

  await page.goto(`${env.url}/settings/agents`);
  await page.getByRole('link', { name: 'Session worktrees on this machine' }).click();
  await expect(page).toHaveURL(`${env.url}/worktrees`);
  const rows = page.getByRole('list', { name: 'Worktrees' });
  const row = (title: string): Locator => rows.locator('li', { hasText: title });
  await expect(row('Clean worktree')).toContainText('clean');
  await expect(row('Dirty worktree')).toContainText('1 uncommitted change');

  // The terminal leaves the registry right after the exit is recorded: prune until it is removed.
  await expect
    .poll(
      async () => {
        await page.getByRole('button', { name: 'Prune' }).click();
        return row('Clean worktree').count();
      },
      { timeout: 15_000 },
    )
    .toBe(0);
  expect(existsSync(clean.worktree)).toBe(false);
  await expect(row('Dirty worktree')).toBeVisible();

  await row('Dirty worktree').getByRole('button', { name: 'Remove…' }).click();
  await page.getByRole('dialog', { name: 'Remove worktree?' }).getByRole('button', { name: 'Remove worktree' }).click();
  const force = page.getByRole('dialog', { name: 'Uncommitted changes' });
  await expect(force).toContainText('uncommitted change');
  expect(existsSync(dirty.worktree)).toBe(true);
  await force.getByRole('button', { name: 'Force remove' }).click();
  await expect(row('Dirty worktree')).toHaveCount(0);
  await expect.poll(() => existsSync(dirty.worktree ?? '')).toBe(false);
});
