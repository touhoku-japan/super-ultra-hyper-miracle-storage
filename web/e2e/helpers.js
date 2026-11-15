// Shared bits of the browser tests: the server's addresses (set by server.js), admin API calls
// for test data, and logging in.
import { expect } from '@playwright/test';
import { randomBytes } from 'node:crypto';

export const S3 = process.env.SUHMS_E2E_S3;
export const ADMIN = process.env.SUHMS_E2E_ADMIN;
export const CONSOLE = process.env.SUHMS_E2E_CONSOLE;
export const TOKEN = process.env.SUHMS_E2E_TOKEN;

/** A unique lowercase suffix, so tests never share tenants or buckets. */
export const uniq = () => randomBytes(4).toString('hex');

async function adminCall(method, path, body) {
  const resp = await fetch(`${ADMIN}${path}`, {
    method,
    headers: { authorization: `Bearer ${TOKEN}`, ...(body ? { 'content-type': 'application/json' } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await resp.text();
  if (!resp.ok) throw new Error(`${method} ${path}: ${resp.status} ${text}`);
  return text ? JSON.parse(text) : null;
}

export const admin = {
  createTenant: (name, limits = {}) => adminCall('POST', '/admin/v1/tenants', { name, ...limits }),
  createKey: (tenantId) => adminCall('POST', `/admin/v1/tenants/${tenantId}/keys`),
  patchTenant: (tenantId, body) => adminCall('PATCH', `/admin/v1/tenants/${tenantId}`, body),
  patchKey: (keyId, body) => adminCall('PATCH', `/admin/v1/keys/${keyId}`, body),
};

/** A new tenant with one access key: `{ tenant, key }`. */
export async function newCustomer(limits) {
  const tenant = await admin.createTenant(`e2e-${uniq()}`, limits);
  const key = await admin.createKey(tenant.id);
  return { tenant, key };
}

/** Logs in to the customer console and waits for the bucket list. */
export async function loginCustomer(page, key) {
  await page.goto(`${CONSOLE}/`);
  await page.getByLabel('アクセスキー ID').fill(key.access_key_id);
  await page.getByLabel('シークレットアクセスキー').fill(key.secret_access_key);
  await page.getByRole('button', { name: 'ログイン' }).click();
  await expect(page.getByRole('heading', { name: 'バケット', level: 1 })).toBeVisible();
}

export async function loginOperator(page) {
  await page.goto(`${ADMIN}/ui/`);
  await page.getByLabel('管理トークン').fill(TOKEN);
  await page.getByRole('button', { name: 'ログイン' }).click();
  await expect(page.getByRole('heading', { name: '概要', level: 1 })).toBeVisible();
}

/** A link in the sidebar. */
export const nav = (page, label) => page.getByRole('complementary').getByRole('link', { name: label, exact: true });

/** The modal that is open right now (native <dialog>). */
export const dialog = (page) => page.locator('dialog[open]');

/** A toast message. */
export const toast = (page, text) => page.getByRole('status').getByText(text);
