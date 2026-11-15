// Admin API (`/admin/v1`), authenticated by the operator session cookie.
import { api } from './api.js';

const B = '/admin/v1';
const e = encodeURIComponent;

export const admin = {
  overview: () => api(`${B}/overview`),
  tenants: () => api(`${B}/tenants`),
  storage: () => api(`${B}/storage`),
  createTenant: (body) => api(`${B}/tenants`, { method: 'POST', body }),
  tenant: (id) => api(`${B}/tenants/${e(id)}`),
  patchTenant: (id, body) => api(`${B}/tenants/${e(id)}`, { method: 'PATCH', body }),
  keys: (id) => api(`${B}/tenants/${e(id)}/keys`),
  createKey: (id) => api(`${B}/tenants/${e(id)}/keys`, { method: 'POST' }),
  patchKey: (key, body) => api(`${B}/keys/${e(key)}`, { method: 'PATCH', body }),
  buckets: (id) => api(`${B}/tenants/${e(id)}/buckets`),
  patchBucket: (name, body) => api(`${B}/buckets/${e(name)}`, { method: 'PATCH', body }),
  usage: (id, from, to) => api(`${B}/tenants/${e(id)}/usage`, { query: { from, to } }),
  disks: () => api(`${B}/disks`),
  drainDisk: (id) => api(`${B}/disks/${e(id)}/drain`, { method: 'POST' }),
  undrainDisk: (id) => api(`${B}/disks/${e(id)}/undrain`, { method: 'POST' }),
  removeDisk: (id, force) => api(`${B}/disks/${e(id)}`, { method: 'DELETE', query: { force: force ? 'true' : undefined } }),
  drainNode: (id) => api(`${B}/nodes/${e(id)}/drain`, { method: 'POST' }),
  undrainNode: (id) => api(`${B}/nodes/${e(id)}/undrain`, { method: 'POST' }),
  removeNode: (id, force) => api(`${B}/nodes/${e(id)}`, { method: 'DELETE', query: { force: force ? 'true' : undefined } }),
  jobs: () => api(`${B}/jobs`),
  startJob: (name, body) => api(`${B}/jobs/${e(name)}`, { method: 'POST', body }),
};
