import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { e2eEnv } from './env';

const env = e2eEnv();
const AUTH = { Authorization: `Bearer ${env.token}` };

async function instrument(page: Page): Promise<void> {
  await page.evaluate(() => {
    const w = window as unknown as { __log: string[] };
    w.__log = [];
    const log = (s: string): void => void w.__log.push(`${performance.now().toFixed(1)} ${s}`);
    const desc = (t: EventTarget | null): string =>
      t instanceof Element ? `${t.tagName}.${String(t.className).slice(0, 40)}#${t.id}` : String(t);
    document.addEventListener('beforetoggle', (e) => log(`beforetoggle ${(e as ToggleEvent).newState} ${desc(e.target)} stack=${new Error().stack?.split('\n').slice(2, 6).join(' | ')}`), true);
    document.addEventListener('toggle', (e) => log(`toggle ${(e as ToggleEvent).oldState}->${(e as ToggleEvent).newState} ${desc(e.target)}`), true);
    for (const t of ['pointerdown', 'pointerup', 'mousedown', 'mouseup', 'contextmenu', 'focusin', 'focusout', 'click', 'auxclick', 'keydown'])
      document.addEventListener(t, (e) => {
        const m = e as MouseEvent;
        log(`${t} btn=${m.button} target=${desc(e.target)} xy=${m.clientX},${m.clientY} prevented=${e.defaultPrevented} active=${desc(document.activeElement)}`);
      }, true);
    for (const name of ['showPopover', 'hidePopover'] as const) {
      const orig = HTMLElement.prototype[name];
      HTMLElement.prototype[name] = function (this: HTMLElement, ...a: unknown[]) {
        log(`${name} ${desc(this)} connected=${this.isConnected} stack=${new Error().stack?.split('\n').slice(2, 6).join(' | ')}`);
        try {
          return (orig as (...x: unknown[]) => void).apply(this, a);
        } catch (err) {
          log(`${name} THREW ${String(err)}`);
          throw err;
        }
      };
    }
    new MutationObserver((ms) => {
      for (const m of ms) for (const n of m.removedNodes) if (n instanceof Element && (n.matches('.menu') || n.querySelector('.menu[aria-label]:not([id=""])'))) log(`removed ${desc(n)}`);
    }).observe(document.body, { childList: true, subtree: true });
    window.addEventListener('error', (e) => log(`error ${e.message}`));
    window.addEventListener('unhandledrejection', (e) => log(`rejection ${String(e.reason)}`));
  });
}

async function dump(page: Page, what: string): Promise<void> {
  const open = await page.locator('.menu:popover-open').count();
  const logs = await page.evaluate(() => (window as unknown as { __log: string[] }).__log.splice(0));
  console.log(`===== ${what}: open menus=${open}\n${logs.join('\n')}`);
}

test('debug grid header menu on linux', async ({ page }) => {
  page.on('console', (m) => console.log(`[console.${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => console.log(`[pageerror] ${e.message}`));
  const api = async (method: string, path: string, data?: unknown): Promise<{ id: string }> => {
    const r = await page.request.fetch(`${env.url}${path}`, { method, data, headers: { Origin: env.url, ...AUTH } });
    const t = await r.text();
    return t ? JSON.parse(t) : undefined;
  };
  await page.goto(`${env.url}/sessions#token=${env.token}`);
  await expect(page).toHaveURL(`${env.url}/sessions`);
  const dir = join(env.root, 'dbg');
  mkdirSync(dir, { recursive: true });
  const p = await api('POST', '/api/projects', { path: dir, name: 'Dbg' });
  const s = await api('POST', '/api/sessions', { agent: 'shell', project_id: p.id });

  for (const withXterm of [false, true]) {
    await page.goto(`${env.url}/grid`);
    const tile = page.locator('section.tile').filter({ has: page.locator(`a[href="/sessions/${s.id}"]`) });
    await tile.locator('.xterm').waitFor();
    await page.waitForTimeout(1500);
    await instrument(page);
    if (withXterm) {
      await tile.locator('.xterm').click({ button: 'right' });
      await page.waitForTimeout(300);
      await dump(page, 'after xterm right-click');
    }
    await tile.locator('header').click({ button: 'right' });
    await page.waitForTimeout(500);
    await dump(page, `header right-click (xterm first: ${withXterm})`);
    await page.keyboard.press('Escape');
    // Same header, pointer well inside the menu's box after it opens.
    await tile.locator('header').click({ button: 'right', position: { x: 300, y: 10 } });
    await page.waitForTimeout(500);
    await dump(page, `header right-click at 300,10 (xterm first: ${withXterm})`);
    await page.keyboard.press('Escape');
  }
  // The sidebar card, which passes in CI.
  await page.goto(`${env.url}/sessions/${s.id}`);
  await page.waitForTimeout(1500);
  await instrument(page);
  await page.getByRole('complementary', { name: 'Sessions' }).locator(`a[href="/sessions/${s.id}"]`).click({ button: 'right' });
  await page.waitForTimeout(500);
  await dump(page, 'card right-click');
  await api('POST', `/api/sessions/${s.id}/stop`);
});
