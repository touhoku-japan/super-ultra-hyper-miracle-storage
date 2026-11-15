// Browser-level checks of the console's defences (the unit of each is tested in Rust; these make
// sure a real browser behaves as intended).
import { test, expect } from '@playwright/test';
import { CONSOLE, loginCustomer, newCustomer, uniq } from './helpers.js';

const CSRF = { 'x-suhms-console': '1' };

test('an uploaded HTML object is downloaded, never rendered with the session', async ({ page }) => {
  const { key } = await newCustomer();
  await loginCustomer(page, key);
  const bucket = `e2e-${uniq()}`;
  expect((await page.request.put(`${CONSOLE}/s3/${bucket}`, { headers: CSRF })).status()).toBe(200);
  const html = '<!doctype html><script>fetch("/api/buckets").then(()=>{document.title="pwned"})</script>';
  const put = await page.request.put(`${CONSOLE}/s3/${bucket}/evil.html`, {
    headers: { ...CSRF, 'content-type': 'text/html' },
    data: html,
  });
  expect(put.status()).toBe(200);

  const resp = await page.request.get(`${CONSOLE}/s3/${bucket}/evil.html`);
  expect(resp.headers()['content-disposition']).toMatch(/^attachment/);
  expect(resp.headers()['content-security-policy']).toContain('sandbox');
  expect(resp.headers()['x-content-type-options']).toBe('nosniff');

  // Opening it in a tab starts a download instead of running the script on the console's origin.
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    page.goto(`${CONSOLE}/s3/${bucket}/evil.html`).catch(() => {}),
  ]);
  expect(download.suggestedFilename()).toBe('evil.html');
  expect(await page.title()).not.toBe('pwned');
});

test('state-changing requests without the console header are refused', async ({ page }) => {
  const { key } = await newCustomer();
  await loginCustomer(page, key);
  const bucket = `e2e-${uniq()}`;
  // What a cross-site form could send: the cookie, but no custom header.
  expect((await page.request.put(`${CONSOLE}/s3/${bucket}`)).status()).toBe(403);
  const presign = await page.request.post(`${CONSOLE}/api/presign`, { data: { bucket, key: 'x', expires_secs: 60 } });
  expect(presign.status()).toBe(403);
  expect((await page.request.get(`${CONSOLE}/api/buckets`)).status()).toBe(200);
});

test('the session cookie is HttpOnly and SameSite=Strict, and the page cannot be framed', async ({ page, context }) => {
  const { key } = await newCustomer();
  await loginCustomer(page, key);
  const cookies = await context.cookies(CONSOLE);
  expect(cookies).toHaveLength(1);
  expect(cookies[0].httpOnly).toBe(true);
  expect(cookies[0].sameSite).toBe('Strict');
  expect(await page.evaluate(() => document.cookie)).toBe('');

  const resp = await page.request.get(`${CONSOLE}/`);
  expect(resp.headers()['content-security-policy']).toContain("frame-ancestors 'none'");
});
