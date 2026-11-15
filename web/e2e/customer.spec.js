// The customer console: logging in, buckets, uploads (single and multipart), the object panel,
// share links, downloads and deletion.
import { test, expect } from '@playwright/test';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { CONSOLE, dialog, loginCustomer, nav, newCustomer, toast, uniq } from './helpers.js';

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');
const fileInput = (page) => page.locator('input[type=file]:not([webkitdirectory])');

test('a wrong secret is rejected with a message', async ({ page }) => {
  const { key } = await newCustomer();
  await page.goto(`${CONSOLE}/`);
  await page.getByLabel('アクセスキー ID').fill(key.access_key_id);
  await page.getByLabel('シークレットアクセスキー').fill('not-the-secret');
  await page.getByRole('button', { name: 'ログイン' }).click();
  await expect(page.getByRole('alert')).toHaveText('アクセスキーまたはシークレットキーが正しくありません');
});

test('bucket, upload, object panel, share link, download and delete', async ({ page, request }) => {
  const { key } = await newCustomer();
  await loginCustomer(page, key);
  await expect(page.getByRole('heading', { name: 'まだバケットがありません' })).toBeVisible();

  // Create a bucket; the console opens it.
  const bucket = `e2e-${uniq()}`;
  await page.getByRole('button', { name: 'バケットを作成' }).first().click();
  await dialog(page).getByLabel('バケット名').fill('Bad_Name');
  await expect(dialog(page).getByText('英小文字・数字・ハイフン・ドットのみ使えます', { exact: false })).toBeVisible();
  await expect(dialog(page).getByRole('button', { name: '作成' })).toBeDisabled();
  await dialog(page).getByLabel('バケット名').fill(bucket);
  await dialog(page).getByRole('button', { name: '作成' }).click();
  await expect(toast(page, `バケット ${bucket} を作成しました`)).toBeVisible();
  await expect(page.getByRole('heading', { name: bucket, level: 1 })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'まだオブジェクトがありません' })).toBeVisible();

  // Upload a file whose name needs encoding.
  const name = '報告書 2026 (最終).txt';
  const body = Buffer.from('こんにちは、蔵の中身です。\n');
  await fileInput(page).setInputFiles({ name, mimeType: 'text/plain', buffer: body });
  await expect(toast(page, '1 件のアップロードが完了しました')).toBeVisible();
  const row = page.getByRole('row').filter({ hasText: name });
  await expect(row).toContainText(`${body.length} B`);

  // The object panel: properties, tags, share link.
  await row.getByRole('button', { name }).click();
  const panel = page.getByRole('dialog', { name });
  await expect(panel.getByText(`${bucket}/${name}`)).toBeVisible();
  await expect(panel.getByText('text/plain')).toBeVisible();
  await expect(panel.getByText('CRC64NVME', { exact: true })).toBeVisible();

  await panel.getByRole('button', { name: 'タグを追加' }).click();
  await panel.getByLabel('タグのキー').fill('project');
  await panel.getByLabel('タグの値').fill('kura');
  await panel.getByRole('button', { name: 'タグを保存' }).click();
  await expect(toast(page, 'タグを保存しました')).toBeVisible();

  await panel.getByRole('button', { name: 'リンクを作成' }).click();
  const shareUrl = await panel.locator('textarea').inputValue();
  expect(shareUrl).toContain('X-Amz-Signature=');
  // Anyone with the link can download it, straight from S3 (no console cookie).
  const shared = await request.get(shareUrl, { headers: { cookie: '' } });
  expect(shared.status()).toBe(200);
  expect(await shared.body()).toEqual(body);

  // Downloads go through the console proxy under the key's own name.
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    panel.getByRole('link', { name: 'ダウンロード' }).click(),
  ]);
  expect(download.suggestedFilename()).toBe(name);
  expect(readFileSync(await download.path())).toEqual(body);

  // The tag survives reopening the panel.
  await panel.getByRole('button', { name: '閉じる' }).click();
  await row.getByRole('button', { name }).click();
  await expect(panel.getByLabel('タグの値')).toHaveValue('kura');
  await panel.getByRole('button', { name: '閉じる' }).click();

  // A folder, navigated into and out of.
  await page.getByRole('button', { name: 'フォルダを作成' }).click();
  await dialog(page).getByLabel('フォルダ名').fill('写真');
  await dialog(page).getByRole('button', { name: '作成' }).click();
  // The console opens the new folder; the breadcrumb leads back and the folder is listed.
  await expect(page.getByRole('heading', { name: '写真', level: 1 })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'このフォルダは空です' })).toBeVisible();
  await page.getByRole('main').getByRole('link', { name: bucket }).click();
  await page.getByRole('link', { name: '写真' }).click();
  await expect(page.getByRole('heading', { name: 'このフォルダは空です' })).toBeVisible();
  await page.getByRole('main').getByRole('link', { name: bucket }).click();

  // Delete everything, then the bucket (which needs its name typed).
  await page.getByRole('checkbox', { name: `${name} を選択` }).check();
  await page.getByRole('button', { name: '選択を削除（1）' }).click();
  await dialog(page).getByRole('button', { name: '削除' }).click();
  await expect(toast(page, '1 件を削除しました')).toBeVisible();
  await page.getByRole('row').filter({ hasText: '写真' }).getByRole('button', { name: '削除' }).click();
  await dialog(page).getByRole('button', { name: '削除' }).click();
  await expect(page.getByRole('heading', { name: 'まだオブジェクトがありません' })).toBeVisible();

  await nav(page, 'バケット').click();
  await page.getByRole('row').filter({ hasText: bucket }).getByRole('button', { name: '削除' }).click();
  const confirm = dialog(page).getByRole('button', { name: '削除' });
  await expect(confirm).toBeDisabled();
  await dialog(page).getByRole('textbox').fill(bucket);
  await confirm.click();
  await expect(toast(page, `バケット ${bucket} を削除しました`)).toBeVisible();
  await expect(page.getByRole('heading', { name: 'まだバケットがありません' })).toBeVisible();

  // Logging out ends the session.
  await page.getByRole('button', { name: 'ログアウト' }).click();
  await expect(page.getByRole('button', { name: 'ログイン' })).toBeVisible();
  const after = await page.request.get(`${CONSOLE}/api/buckets`);
  expect(after.status()).toBe(401);
});

test('a large file is uploaded in parts and reads back intact', async ({ page, request }, testInfo) => {
  test.slow();
  const { key } = await newCustomer();
  const bucket = `e2e-${uniq()}`;
  await loginCustomer(page, key);
  await page.getByRole('button', { name: 'バケットを作成' }).first().click();
  await dialog(page).getByLabel('バケット名').fill(bucket);
  await dialog(page).getByRole('button', { name: '作成' }).click();
  await expect(page.getByRole('heading', { name: bucket, level: 1 })).toBeVisible();

  // 65 MiB: over the 64 MiB threshold, so the console uses multipart (8 MiB parts -> 9 parts).
  const size = 65 * 1024 * 1024;
  const data = Buffer.alloc(size);
  for (let i = 0; i < size; i += 4) data.writeUInt32LE((i * 2654435761) >>> 0, i);
  const path = testInfo.outputPath('large.bin');
  writeFileSync(path, data);

  await fileInput(page).setInputFiles(path);
  await expect(toast(page, '1 件のアップロードが完了しました')).toBeVisible({ timeout: 120_000 });
  const row = page.getByRole('row').filter({ hasText: 'large.bin' });
  await expect(row).toContainText('65 MiB');

  await row.getByRole('button', { name: 'large.bin' }).click();
  const panel = page.getByRole('dialog', { name: 'large.bin' });
  // A multipart ETag ends in -<parts>.
  await expect(panel.locator('dd.mono').filter({ hasText: /-9"?$/ })).toBeVisible();
  await panel.getByRole('button', { name: 'リンクを作成' }).click();
  const got = await request.get(await panel.locator('textarea').inputValue());
  expect(got.status()).toBe(200);
  expect(sha256(await got.body())).toBe(sha256(data));
});

test('the bucket limit disables creating more buckets', async ({ page }) => {
  const { key } = await newCustomer({ max_buckets: 1 });
  await loginCustomer(page, key);
  await page.getByRole('button', { name: 'バケットを作成' }).first().click();
  await dialog(page).getByLabel('バケット名').fill(`e2e-${uniq()}`);
  await dialog(page).getByRole('button', { name: '作成' }).click();
  await nav(page, 'バケット').click();
  await expect(page.getByText('1 個（上限 1 個）')).toBeVisible();
  await expect(page.getByRole('button', { name: 'バケットを作成' })).toBeDisabled();
});
