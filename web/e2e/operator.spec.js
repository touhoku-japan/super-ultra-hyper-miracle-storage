// The operator console: logging in with the admin token, creating a tenant and its key (which
// then works in the customer console), suspending it, the cluster view and maintenance jobs.
import { test, expect } from '@playwright/test';
import { ADMIN, dialog, loginCustomer, loginOperator, nav, toast, uniq } from './helpers.js';

test('a wrong admin token is rejected', async ({ page }) => {
  await page.goto(`${ADMIN}/ui/`);
  await page.getByLabel('管理トークン').fill('x'.repeat(40));
  await page.getByRole('button', { name: 'ログイン' }).click();
  await expect(page.getByRole('alert')).toBeVisible();
  await expect(page.getByRole('heading', { name: '概要', level: 1 })).toHaveCount(0);
});

test('overview and cluster show the three disks', async ({ page }) => {
  await loginOperator(page);
  await expect(page.getByRole('heading', { name: 'ディスク' })).toBeVisible();
  await nav(page, 'ディスクとノード').click();
  await expect(page.getByRole('heading', { name: 'ディスクとノード', level: 1 })).toBeVisible();
  for (const d of ['d1', 'd2', 'd3']) {
    await expect(page.getByRole('cell', { name: new RegExp(`/${d} `) })).toHaveCount(1);
  }
});

test('a tenant created here can log in to the customer console until suspended', async ({ page, browser }) => {
  const name = `e2e-op-${uniq()}`;
  await loginOperator(page);
  await nav(page, 'テナント').click();
  await page.getByRole('button', { name: 'テナントを作成' }).click();
  await dialog(page).getByLabel('名前').fill(name);
  await dialog(page).getByLabel('容量上限').fill('10 GiB');
  await dialog(page).getByRole('button', { name: '作成' }).click();
  await expect(toast(page, `テナント ${name} を作成しました`)).toBeVisible();

  // The console opens the new tenant.
  await expect(page.getByRole('heading', { name, level: 1 })).toBeVisible();
  await expect(page.getByLabel('容量上限')).toHaveValue('10 GiB');

  // The secret is shown once, in a dialog.
  await page.getByRole('button', { name: 'アクセスキーを発行' }).click();
  const values = dialog(page).locator('dd.mono');
  const firstText = (el) => el.firstChild.textContent.trim();
  const key = {
    access_key_id: await values.nth(0).evaluate(firstText),
    secret_access_key: await values.nth(1).evaluate(firstText),
  };
  expect(key.access_key_id).toMatch(/^[A-Z0-9]{20}$/);
  await dialog(page).getByRole('button', { name: '控えました' }).click();
  await expect(page.getByRole('row').filter({ hasText: key.access_key_id })).toContainText('有効');

  // The customer logs in with it (separate browser context = separate cookies).
  const customerContext = await browser.newContext();
  const customer = await customerContext.newPage();
  await loginCustomer(customer, key);

  // Suspending needs the tenant's name typed.
  await page.getByRole('button', { name: 'テナントを停止' }).click();
  await dialog(page).getByRole('textbox').fill(name);
  await dialog(page).getByRole('button', { name: '停止' }).click();
  await expect(toast(page, 'テナントを停止しました')).toBeVisible();
  await expect(page.getByText('このテナントは停止中です', { exact: false })).toBeVisible();

  // After a reload the login screen says why the session ended.
  await customer.reload();
  await expect(customer.getByRole('alert')).toHaveText('このアカウントは停止されています');
  await expect(customer.getByRole('button', { name: 'ログイン' })).toBeVisible();

  await page.getByRole('button', { name: '停止を解除' }).click();
  await expect(toast(page, 'テナントの停止を解除しました')).toBeVisible();
  // The cookie is still valid, so the console comes back without logging in again.
  await customer.reload();
  await expect(customer.getByRole('heading', { name: 'まだバケットがありません' })).toBeVisible();

  // Disabling the key ends the customer's session.
  await page.getByRole('row').filter({ hasText: key.access_key_id }).getByRole('button', { name: '無効にする' }).click();
  await dialog(page).getByRole('button', { name: '無効にする' }).click();
  await expect(toast(page, 'アクセスキーを無効にしました')).toBeVisible();
  await customer.reload();
  await expect(customer.getByRole('button', { name: 'ログイン' })).toBeVisible();
  await customerContext.close();
});

test('a maintenance job runs and shows its result', async ({ page }) => {
  await loginOperator(page);
  await nav(page, 'メンテナンス').click();
  const card = page.locator('section.sheet').filter({ has: page.getByRole('heading', { name: 'scrub（全コピーの検証）' }) });
  await card.getByRole('button', { name: '今すぐ実行' }).click();
  await expect(toast(page, 'scrub（全コピーの検証） を開始しました')).toBeVisible();
  await expect(card.getByText('前回', { exact: false })).toBeVisible({ timeout: 30_000 });
  await expect(card.locator('dl.result dt').first()).toBeVisible();
});

test('logging out of the operator console ends the session', async ({ page }) => {
  await loginOperator(page);
  await page.getByRole('button', { name: 'ログアウト' }).click();
  await expect(page.getByLabel('管理トークン')).toBeVisible();
  expect(await (await page.request.get(`${ADMIN}/ui/api/session`)).json()).toMatchObject({ logged_in: false });
  expect((await page.request.get(`${ADMIN}/admin/v1/overview`)).status()).toBe(401);
});
