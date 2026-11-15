// Pure helpers for the S3 client: URL encoding and XML request bodies (testable in Node).

/** Percent-encodes a key for a URL path, keeping `/` separators. */
export function encodeKey(key) {
  return key
    .split('/')
    .map((s) => encodeURIComponent(s).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`))
    .join('/');
}

/** `?a=1&b` from an object (values `true` become bare keys; empty/undefined are dropped). */
export function queryString(q) {
  if (!q) return '';
  const parts = [];
  for (const [k, v] of Object.entries(q)) {
    if (v === undefined || v === null || v === false) continue;
    parts.push(v === true || v === '' ? encodeURIComponent(k) : `${encodeURIComponent(k)}=${encodeURIComponent(v)}`);
  }
  return parts.length ? `?${parts.join('&')}` : '';
}

export function esc(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&apos;');
}

const tag = (name, value) => (value === undefined || value === null || value === '' ? '' : `<${name}>${esc(value)}</${name}>`);

export function completeMultipartXml(parts) {
  return `<CompleteMultipartUpload>${parts
    .map((p) => `<Part><PartNumber>${p.partNumber}</PartNumber><ETag>${esc(p.etag)}</ETag></Part>`)
    .join('')}</CompleteMultipartUpload>`;
}

export function deleteObjectsXml(objects) {
  return `<Delete><Quiet>true</Quiet>${objects
    .map((o) => `<Object>${tag('Key', o.key)}${tag('VersionId', o.versionId)}</Object>`)
    .join('')}</Delete>`;
}

export function versioningXml(status) {
  return `<VersioningConfiguration>${tag('Status', status)}</VersioningConfiguration>`;
}

export function taggingXml(tags) {
  return `<Tagging><TagSet>${tags
    .map((t) => `<Tag>${tag('Key', t.key)}<Value>${esc(t.value ?? '')}</Value></Tag>`)
    .join('')}</TagSet></Tagging>`;
}

/**
 * A lifecycle rule as the console edits it:
 * { id, enabled, prefix, tags: [{key, value}], sizeGt, sizeLt,
 *   expirationDays, expirationDate, expiredObjectDeleteMarker,
 *   noncurrentDays, newerNoncurrentVersions,
 *   transitions: [{days, date, storageClass}], noncurrentTransitions: [{days, storageClass}],
 *   abortDays }
 */
export function lifecycleXml(rules) {
  return `<LifecycleConfiguration>${rules.map(lifecycleRuleXml).join('')}</LifecycleConfiguration>`;
}

function filterXml(r) {
  const conds = [];
  if (r.prefix) conds.push(tag('Prefix', r.prefix));
  for (const t of r.tags ?? []) conds.push(`<Tag>${tag('Key', t.key)}<Value>${esc(t.value ?? '')}</Value></Tag>`);
  if (num(r.sizeGt) !== null) conds.push(tag('ObjectSizeGreaterThan', num(r.sizeGt)));
  if (num(r.sizeLt) !== null) conds.push(tag('ObjectSizeLessThan', num(r.sizeLt)));
  if (conds.length === 0) return '<Filter></Filter>';
  if (conds.length === 1) return `<Filter>${conds[0]}</Filter>`;
  return `<Filter><And>${conds.join('')}</And></Filter>`;
}

function num(v) {
  if (v === undefined || v === null || v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

/** "2027-01-01" -> "2027-01-01T00:00:00.000Z" (S3 wants midnight UTC). */
export function lifecycleDate(d) {
  if (!d) return null;
  return d.length === 10 ? `${d}T00:00:00.000Z` : d;
}

function lifecycleRuleXml(r) {
  let x = '<Rule>';
  x += tag('ID', r.id);
  x += filterXml(r);
  x += tag('Status', r.enabled ? 'Enabled' : 'Disabled');
  for (const t of r.transitions ?? []) {
    if (num(t.days) === null && !t.date) continue;
    x += `<Transition>${num(t.days) !== null ? tag('Days', num(t.days)) : tag('Date', lifecycleDate(t.date))}${tag('StorageClass', t.storageClass)}</Transition>`;
  }
  for (const t of r.noncurrentTransitions ?? []) {
    if (num(t.days) === null) continue;
    x += `<NoncurrentVersionTransition>${tag('NoncurrentDays', num(t.days))}${tag('StorageClass', t.storageClass)}</NoncurrentVersionTransition>`;
  }
  if (num(r.expirationDays) !== null) x += `<Expiration>${tag('Days', num(r.expirationDays))}</Expiration>`;
  else if (r.expirationDate) x += `<Expiration>${tag('Date', lifecycleDate(r.expirationDate))}</Expiration>`;
  else if (r.expiredObjectDeleteMarker) x += '<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>';
  if (num(r.noncurrentDays) !== null) {
    x += `<NoncurrentVersionExpiration>${tag('NoncurrentDays', num(r.noncurrentDays))}${tag('NewerNoncurrentVersions', num(r.newerNoncurrentVersions))}</NoncurrentVersionExpiration>`;
  }
  if (num(r.abortDays) !== null) {
    x += `<AbortIncompleteMultipartUpload>${tag('DaysAfterInitiation', num(r.abortDays))}</AbortIncompleteMultipartUpload>`;
  }
  return `${x}</Rule>`;
}

/** { id, origins: [], methods: [], headers: [], expose: [], maxAge } */
export function corsXml(rules) {
  return `<CORSConfiguration>${rules
    .map((r) => {
      let x = '<CORSRule>';
      x += tag('ID', r.id);
      for (const o of r.origins ?? []) x += tag('AllowedOrigin', o);
      for (const m of r.methods ?? []) x += tag('AllowedMethod', m);
      for (const h of r.headers ?? []) x += tag('AllowedHeader', h);
      for (const h of r.expose ?? []) x += tag('ExposeHeader', h);
      if (num(r.maxAge) !== null) x += tag('MaxAgeSeconds', num(r.maxAge));
      return `${x}</CORSRule>`;
    })
    .join('')}</CORSConfiguration>`;
}

/** mode: null (no default) | 'GOVERNANCE' | 'COMPLIANCE'; days or years. */
export function objectLockXml({ mode, days, years }) {
  let rule = '';
  if (mode) {
    const period = num(years) !== null ? tag('Years', num(years)) : tag('Days', num(days));
    rule = `<Rule><DefaultRetention>${tag('Mode', mode)}${period}</DefaultRetention></Rule>`;
  }
  return `<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>${rule}</ObjectLockConfiguration>`;
}

export function retentionXml(mode, until) {
  return `<Retention>${tag('Mode', mode)}${tag('RetainUntilDate', until)}</Retention>`;
}

export function legalHoldXml(on) {
  return `<LegalHold><Status>${on ? 'ON' : 'OFF'}</Status></LegalHold>`;
}

/** `Content-Disposition` value that downloads under the key's own name. */
export function attachment(name) {
  const ascii = name.replace(/[^\x20-\x7e]|["\\]/g, '_');
  return `attachment; filename="${ascii}"; filename*=UTF-8''${encodeURIComponent(name)}`;
}

/** Splits a file into multipart parts: at least 8 MiB, at most 10 000 parts. */
export function partSize(total) {
  const min = 8 * 1024 * 1024;
  return Math.max(min, Math.ceil(total / 10000 / (1024 * 1024)) * 1024 * 1024);
}

/** S3 bucket naming rules (lowercase DNS label style, 3-63 chars). Returns an error or null. */
export function bucketNameError(name) {
  if (name.length < 3 || name.length > 63) return '3〜63 文字にしてください';
  if (!/^[a-z0-9][a-z0-9.-]*[a-z0-9]$/.test(name)) return '英小文字・数字・ハイフン・ドットのみ使えます（先頭と末尾は英小文字か数字）';
  if (name.includes('..')) return 'ドットを続けて使うことはできません';
  if (/^\d+\.\d+\.\d+\.\d+$/.test(name)) return 'IP アドレスの形式は使えません';
  return null;
}
