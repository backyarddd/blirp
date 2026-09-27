// The version and the update notice, against the daemon's check of the fake releases API that
// global-setup serves (FAKE_RELEASE). The debug daemon has no install receipt, so it offers
// "How to update" instead of "Update now"; nothing here runs an update.
import { expect, test, type Page } from '@playwright/test';
import { FAKE_RELEASE, e2eEnv } from './env';

const env = e2eEnv();

test.describe.configure({ mode: 'serial' });

let page: Page;
let version = '';

test.beforeAll(async ({ browser }) => {
  page = await (await browser.newContext()).newPage();
  const health = await page.request.get(`${env.url}/api/health`, { headers: { Authorization: `Bearer ${env.token}` } });
  version = ((await health.json()) as { version: string }).version;
  await page.goto(`${env.url}/sessions#token=${env.token}`);
  await expect(page.getByRole('heading', { name: 'Pick a session' })).toBeVisible();
});

test.afterAll(async () => {
  await page?.context().close();
});

test('shows the version on the logo and in the settings navigation', async () => {
  await expect(page.getByTestId('logo')).toHaveAttribute('title', `blirp ${version}`);
  await page.getByRole('link', { name: 'Settings' }).click();
  await expect(page.getByTestId('settings-version')).toHaveText(`blirp ${version}`);
});

test('announces the newer release with a dot and a dismissible banner', async () => {
  const settings = page.getByRole('link', { name: `Settings (blirp ${FAKE_RELEASE} is available)` });
  await expect(settings).toBeVisible();
  await expect(page.getByTestId('update-dot')).toBeVisible();
  const banner = page.getByTestId('update-banner');
  await expect(banner).toContainText(`blirp ${FAKE_RELEASE} is available`);
  // Not a script install: point at the release page, no Update now.
  await expect(banner.getByRole('link', { name: 'How to update' })).toHaveAttribute(
    'href',
    `https://example.invalid/releases/v${FAKE_RELEASE}`,
  );
  await expect(banner.getByRole('button', { name: 'Update now' })).toHaveCount(0);

  await banner.getByRole('button', { name: 'Dismiss' }).click();
  await expect(banner).toBeHidden();
  // Remembered for this version; the dot stays.
  await page.reload();
  await expect(page.getByTestId('update-dot')).toBeVisible();
  await expect(page.getByTestId('update-banner')).toBeHidden();
  expect(await page.evaluate(() => localStorage.getItem('blirp.update.dismissed'))).toBe(FAKE_RELEASE);
});

test('Settings > About checks again on request', async () => {
  await page.goto(`${env.url}/settings/about`);
  const card = page.getByTestId('about-updates');
  await expect(card).toContainText(`blirp ${FAKE_RELEASE} is available`);
  await expect(card).toContainText('not installed by the install script');
  await card.getByRole('button', { name: 'Check now' }).click();
  await expect(card.getByRole('button', { name: 'Check now' })).toBeEnabled();
  await expect(card).toContainText('Checked just now');
});
