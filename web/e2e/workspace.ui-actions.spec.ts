// Session and project actions: context menus (never over a terminal), F2 rename, archive, bulk
// move with a partial failure, and a project's round trip through the Trash. Runs against the same
// daemon as the other specs, in its own folders and projects. Tests share one page and run in order.
import { mkdirSync } from 'node:fs';
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
