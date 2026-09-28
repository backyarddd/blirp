// Terminal fidelity against the real daemon and PTY (ConPTY on Windows): every key reaches the
// program as xterm.js encodes it, app shortcuts take only their allowlist (shortcuts.ts), and a
// reattach (reload, switching sessions) keeps the modes the program set: kitty keyboard flags,
// application cursor keys, bracketed paste, mouse and focus reporting. The programs are
// key-echo.mjs agents (global-setup.ts) that log every input byte they receive.
//
// Runs after workspace.spec.ts (files run in name order): its sessions start in that spec's Plain
// Folder project instead of adding a project of their own.
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { e2eEnv } from './env';

const env = e2eEnv();
const isWindows = process.platform === 'win32';
const AUTH = { Authorization: `Bearer ${env.token}` };

type Mode = 'plain' | 'app' | 'kitty';

test.describe.configure({ mode: 'serial' });

let page: Page;
const sessions: Partial<Record<Mode, string>> = {};

/** The chunks the `mode` agent received so far, hex encoded, after its "ready" line. */
function received(mode: Mode): string[] {
  const file = join(env.root, `keys-${mode}.log`);
  if (!existsSync(file)) return [];
  const lines = readFileSync(file, 'utf8').split('\n').filter(Boolean);
  return lines.slice(lines.lastIndexOf('ready') + 1);
}

const hex = (s: string): string => Buffer.from(s, 'utf8').toString('hex');

/** Run `action` and expect exactly `want` (hex) to reach the program; '' means nothing. */
async function expectBytes(mode: Mode, action: () => Promise<void>, want: string, what: string): Promise<void> {
  const before = received(mode).length;
  await action();
  if (want === '') {
    await page.waitForTimeout(500);
    expect(received(mode).slice(before).join(''), what).toBe('');
    return;
  }
  await expect.poll(() => received(mode).slice(before).join(''), { message: what, timeout: 5_000 }).toBe(want);
}

async function launch(mode: Mode): Promise<string> {
  const res = await page.request.post(`${env.url}/api/sessions`, {
    data: { agent: `custom:keys-${mode}`, cwd: env.plain },
    headers: { Origin: env.url, ...AUTH },
  });
  expect(res.ok(), await res.text()).toBe(true);
  const { id } = (await res.json()) as { id: string };
  await expect.poll(() => existsSync(join(env.root, `keys-${mode}.log`)) && readFileSync(join(env.root, `keys-${mode}.log`), 'utf8').includes('ready'), { timeout: 30_000 }).toBe(true);
  sessions[mode] = id;
  return id;
}

/** Open the session's pane and focus its terminal without a click (a click is mouse input). */
async function show(mode: Mode): Promise<void> {
  const id = sessions[mode] ?? (await launch(mode));
  if (!page.url().endsWith(`/sessions/${id}`)) await page.goto(`${env.url}/sessions/${id}`);
  await page.locator('.xterm').waitFor();
  await expect(page.getByText('Connecting to terminal…')).toHaveCount(0);
  await focusTerminal();
}

async function focusTerminal(): Promise<void> {
  await page.locator('.xterm-helper-textarea').focus();
  // Focus reports a program asked for are sent on focus; let them land first.
  await page.waitForTimeout(200);
}

/** Size of the session's PTY, from the snapshot a second attach gets. */
function ptySize(id: string): Promise<{ cols: number; rows: number }> {
  return page.evaluate(async (sid) => {
    const path = `/api/terminals/${sid}/ws`;
    const res = await fetch('/api/ws-ticket', {
      method: 'POST',
      headers: { Authorization: `Bearer ${localStorage.getItem('blirp.token') ?? ''}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    });
    if (!res.ok) throw new Error(`ws-ticket: ${res.status}`);
    const { ticket } = (await res.json()) as { ticket: string };
    return new Promise<{ cols: number; rows: number }>((resolve, reject) => {
      const ws = new WebSocket(`ws://${location.host}${path}?ticket=${ticket}`);
      const timer = setTimeout(() => reject(new Error('no snapshot within 10 s')), 10_000);
      ws.onmessage = (ev) => {
        if (typeof ev.data !== 'string') return;
        const m = JSON.parse(ev.data) as { type: string; cols: number; rows: number };
        if (m.type !== 'snapshot') return;
        clearTimeout(timer);
        ws.close();
        resolve({ cols: m.cols, rows: m.rows });
      };
      ws.onerror = () => reject(new Error('terminal websocket failed'));
    });
  }, id);
}

test.beforeAll(async ({ browser }) => {
  const context = await browser.newContext({ permissions: ['clipboard-read', 'clipboard-write'] });
  page = await context.newPage();
  await page.goto(`${env.url}/sessions#token=${env.token}`);
  await expect(page).toHaveURL(`${env.url}/sessions`);
});

test.afterAll(async () => {
  for (const id of Object.values(sessions)) {
    await page.request.post(`${env.url}/api/sessions/${id}/stop`, { headers: { Origin: env.url, ...AUTH } });
  }
  await page?.context().close();
});

// What xterm.js sends for each key without any mode set (as VS Code's terminal does).
const LEGACY: [string, string][] = [
  ['ArrowUp', '1b5b41'],
  ['ArrowDown', '1b5b42'],
  ['ArrowRight', '1b5b43'],
  ['ArrowLeft', '1b5b44'],
  ['Home', '1b5b48'],
  ['End', '1b5b46'],
  ['PageUp', '1b5b357e'],
  ['PageDown', '1b5b367e'],
  ['Insert', '1b5b327e'],
  ['Delete', '1b5b337e'],
  ['F1', '1b4f50'],
  ['F5', '1b5b31357e'],
  ['F12', '1b5b32347e'],
  ['Escape', '1b'],
  ['Tab', '09'],
  ['Shift+Tab', '1b5b5a'],
  ['Enter', '0d'],
  ['Alt+Enter', '1b0d'],
  ['Backspace', '7f'],
  ['Control+Backspace', '08'],
  ['Alt+Backspace', '1b7f'],
  ['Control+c', '03'],
  ['Control+d', '04'],
  ['Control+r', '12'],
  ['Control+o', '0f'],
  ['Control+k', '0b'],
  ['Control+w', '17'],
  ['Control+t', '14'],
  ['Control+g', '07'],
  ['Control+l', '0c'],
  ['Control+z', '1a'],
  ['Control+a', '01'],
  ['Control+e', '05'],
  ['Control+u', '15'],
  ['Control+p', '10'],
  ['Control+n', '0e'],
  ['Control+j', '0a'],
  ['Control+ArrowLeft', '1b5b313b3544'],
  ['Control+ArrowRight', '1b5b313b3543'],
  ['Control+Shift+ArrowLeft', '1b5b313b3644'],
  ['Control+Shift+ArrowRight', '1b5b313b3643'],
  ['Shift+ArrowUp', '1b5b313b3241'],
  ['Alt+ArrowLeft', '1b5b313b3344'],
  ['Shift+Home', '1b5b313b3248'],
  ['Alt+b', '1b62'],
  ['Alt+f', '1b66'],
  ['a', '61'],
  ['Shift+A', '41'],
];

test('every key reaches the program as a terminal sends it', async () => {
  await show('plain');
  for (const [key, want] of LEGACY) {
    await expectBytes('plain', () => page.keyboard.press(key), want, key);
  }
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden();
});

// Pushed by the program before any client attached, so only the snapshot can carry it.
const KITTY: [string, string][] = [
  ['Shift+Enter', '1b5b31333b3275'],
  ['Control+Enter', '1b5b31333b3575'],
  ['Alt+Enter', '1b5b31333b3375'],
  ['Escape', '1b5b323775'],
  ['Control+c', '1b5b39393b3575'],
  ['Control+Backspace', '1b5b3132373b3575'],
  ['Alt+b', '1b5b39383b3375'],
  ['Shift+Tab', '1b5b393b3275'],
  ['Enter', '0d'],
  ['ArrowUp', '1b5b41'],
  ['a', '61'],
];

async function expectKitty(when: string): Promise<void> {
  for (const [key, want] of KITTY) {
    await expectBytes('kitty', () => page.keyboard.press(key), want, `${key} ${when}`);
  }
}

test('kitty keyboard flags reach the terminal and survive a reload and a session switch', async () => {
  if (!sessions.plain) await launch('plain');
  await show('kitty');
  await expectKitty('on first attach');
  await page.reload();
  await show('kitty');
  await expectKitty('after a reload');
  await show('plain');
  await show('kitty');
  await expectKitty('after switching sessions');
});

/** Application cursor keys, bracketed paste, focus and SGR mouse reports. */
async function expectAppModes(when: string): Promise<void> {
  await expectBytes('app', () => page.keyboard.press('ArrowUp'), '1b4f41', `ArrowUp ${when}`);
  await expectBytes('app', () => page.keyboard.press('ArrowLeft'), '1b4f44', `ArrowLeft ${when}`);
  await page.evaluate(() => navigator.clipboard.writeText('pasted text'));
  await expectBytes(
    'app',
    () => page.keyboard.press(isWindows ? 'Control+v' : 'Control+Shift+V'),
    `1b5b3230307e${hex('pasted text')}1b5b3230317e`,
    `bracketed paste ${when}`,
  );
  await expectBytes(
    'app',
    async () => {
      await page.locator('.xterm-helper-textarea').blur();
      await page.waitForTimeout(100);
      await focusTerminal();
    },
    '1b5b4f1b5b49',
    `focus out and in ${when}`,
  );
  const screen = page.locator('.xterm-screen');
  const box = await screen.boundingBox();
  if (!box) throw new Error('no terminal screen');
  // Top-left cell: SGR press and release of button 0 at column 1, row 1.
  await expectBytes('app', () => page.mouse.click(box.x + 2, box.y + 2), '1b5b3c303b313b314d1b5b3c303b313b316d', `mouse ${when}`);
}

test('terminal modes survive a reload and a session switch', async () => {
  await show('app');
  await expectAppModes('on first attach');
  await page.reload();
  await show('app');
  await expectAppModes('after a reload');
  await show('plain');
  await show('app');
  await expectAppModes('after switching sessions');
});

test('app shortcuts in a terminal take only their allowlist and hand focus back', async () => {
  await show('plain');
  // Palette (Ctrl+Shift+K): nothing reaches the program; after Escape keys go to the terminal again.
  await expectBytes('plain', () => page.keyboard.press('Control+Shift+K'), '', 'palette');
  const palette = page.getByRole('dialog', { name: 'Command palette' });
  await expect(palette).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(palette).toBeHidden();
  await expectBytes('plain', () => page.keyboard.press('x'), '78', 'typing after the palette closed');
  // Find (Ctrl+F), as in VS Code; Escape closes it and refocuses the terminal.
  await expectBytes('plain', () => page.keyboard.press('Control+f'), '', 'find');
  const find = page.getByTestId('terminal-find');
  await expect(find).toBeVisible();
  await page.keyboard.type('ready');
  await expect(find.getByRole('status')).toHaveText('1 of 1');
  await page.keyboard.press('Escape');
  await expect(find).toBeHidden();
  await expectBytes('plain', () => page.keyboard.press('y'), '79', 'typing after find closed');
  // Ctrl+PageDown switches sessions (VS Code's next editor) instead of reaching the program.
  const here = page.url();
  await expectBytes('plain', () => page.keyboard.press('Control+PageDown'), '', 'next session');
  await expect(page).not.toHaveURL(here);
});

test('copy takes the selection and otherwise leaves Ctrl+C to the program', async () => {
  await show('plain');
  const box = await page.locator('.xterm-screen').boundingBox();
  if (!box) throw new Error('no terminal screen');
  // "key-echo plain ready" is the first line: double-click its first word.
  await page.mouse.dblclick(box.x + 12, box.y + 6);
  await page.evaluate(() => navigator.clipboard.writeText(''));
  await expectBytes('plain', () => page.keyboard.press(isWindows ? 'Control+c' : 'Control+Shift+C'), '', 'copy');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('key-echo');
  if (isWindows) {
    // Copying cleared the selection, so the next Ctrl+C interrupts.
    await expectBytes('plain', () => page.keyboard.press('Control+c'), '03', 'Ctrl+C without a selection');
  } else {
    await page.keyboard.press('Escape');
    await expectBytes('plain', () => page.keyboard.press('Control+c'), '03', 'Ctrl+C');
  }
});

// The grid's smaller tile resizes the PTY; back in the session view the snapshot comes laid out
// for the tile, and both the pane and the PTY must return to the pane's own size.
test('the pane and the PTY take the pane size back after the grid view', async () => {
  await show('plain');
  const id = sessions.plain ?? '';
  const screenWidth = async (): Promise<number> => (await page.locator('.xterm-screen').boundingBox())?.width ?? 0;
  const single = await ptySize(id);
  const width = await screenWidth();
  await page.goto(`${env.url}/grid`);
  await expect.poll(async () => JSON.stringify(await ptySize(id)), { timeout: 15_000 }).not.toBe(JSON.stringify(single));
  await page.goto(`${env.url}/sessions/${id}`);
  await page.locator('.xterm').waitFor();
  await expect.poll(async () => ptySize(id), { timeout: 15_000 }).toEqual(single);
  await expect.poll(screenWidth, { timeout: 5_000 }).toBe(width);
});
