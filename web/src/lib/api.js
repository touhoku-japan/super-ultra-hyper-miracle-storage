// JSON calls to the console backends. Every state-changing request carries the CSRF header the
// server requires (a cross-site form cannot set it); the session itself is an HttpOnly cookie.

export const CSRF = { 'x-suhms-console': '1' };

export class ApiError extends Error {
  constructor(status, message, body) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

/** Calls `url` and returns the parsed JSON (or null for an empty body). */
export async function api(url, { method = 'GET', body, query } = {}) {
  let u = url;
  if (query) {
    const q = Object.entries(query)
      .filter(([, v]) => v !== undefined && v !== null && v !== '')
      .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
      .join('&');
    if (q) u += `?${q}`;
  }
  const headers = { ...CSRF };
  if (body !== undefined) headers['content-type'] = 'application/json';
  const resp = await fetch(u, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: 'same-origin',
  });
  const text = await resp.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!resp.ok) {
    const msg = (data && data.error) || `${resp.status} ${resp.statusText}`;
    if (resp.status === 401) sessionExpired();
    throw new ApiError(resp.status, msg, data);
  }
  return data;
}

let onExpired = () => {};
/** Registers what to do when the server says the session is gone. */
export function setSessionExpiredHandler(f) {
  onExpired = f;
}
export function sessionExpired() {
  onExpired();
}

/** Session endpoints, relative to the page (`/api/...` or `/ui/api/...`). */
export const session = {
  get: () => api('api/session'),
  login: (body) => api('api/login', { method: 'POST', body }),
  logout: () => api('api/logout', { method: 'POST' }),
};
