// S3 client for the customer console. Requests go to the console's `/s3/...` proxy, which signs
// them with the logged-in access key; the browser never sees the secret.

import { CSRF, sessionExpired } from './api.js';
import {
  attachment,
  completeMultipartXml,
  corsXml,
  deleteObjectsXml,
  encodeKey,
  legalHoldXml,
  lifecycleXml,
  objectLockXml,
  partSize,
  queryString,
  retentionXml,
  taggingXml,
  versioningXml,
} from './s3xml.js';

const ROOT = '/s3';

export class S3Error extends Error {
  constructor(status, code, message) {
    super(message || code || `HTTP ${status}`);
    this.status = status;
    this.code = code;
  }
}

/** Japanese explanation of common S3 error codes. */
const MESSAGES = {
  AccessDenied: 'アクセスが拒否されました',
  BucketAlreadyExists: 'そのバケット名は既に使われています',
  BucketAlreadyOwnedByYou: 'そのバケットは既にあります',
  BucketNotEmpty: 'バケットが空ではありません。先にオブジェクト（すべてのバージョン）を削除してください',
  InvalidBucketName: 'バケット名が不正です（使えない名前か、予約されています）',
  NoSuchBucket: 'バケットが見つかりません',
  NoSuchKey: 'オブジェクトが見つかりません',
  QuotaExceeded: '容量の上限を超えます',
  TooManyBuckets: 'バケット数の上限に達しています',
  SlowDown: 'リクエストが多すぎます。少し待ってからやり直してください',
  InvalidStorageClass: '使えないストレージクラスです',
  EntityTooLarge: 'ファイルが大きすぎます',
};

export function errorMessage(e) {
  if (e instanceof S3Error) return MESSAGES[e.code] ? `${MESSAGES[e.code]}（${e.code}）` : e.message;
  return e?.message ?? String(e);
}

function url(bucket, key, query) {
  let p = ROOT + '/';
  if (bucket) p += encodeURIComponent(bucket);
  if (key !== undefined && key !== null) p += '/' + encodeKey(key);
  return p + queryString(query);
}

function parse(text) {
  return new DOMParser().parseFromString(text, 'application/xml');
}

/** Direct children of `el` named `name`. */
function kids(el, name) {
  return el ? Array.from(el.children).filter((c) => c.localName === name) : [];
}
function kid(el, name) {
  return kids(el, name)[0] ?? null;
}
function text(el, name) {
  const k = kid(el, name);
  return k ? k.textContent : null;
}
function int(el, name) {
  const t = text(el, name);
  return t === null ? null : Number(t);
}

async function failure(resp) {
  if (resp.status === 401) {
    sessionExpired();
    return new S3Error(401, 'Unauthorized', 'ログインしてください');
  }
  const body = await resp.text().catch(() => '');
  let code = null;
  let message = null;
  if (body.startsWith('<')) {
    const doc = parse(body);
    code = doc.querySelector('Code')?.textContent ?? null;
    message = doc.querySelector('Message')?.textContent ?? null;
  } else if (body.startsWith('{')) {
    try {
      message = JSON.parse(body).error;
    } catch {
      /* not JSON */
    }
  }
  return new S3Error(resp.status, code, message || `${resp.status} ${resp.statusText}`);
}

async function request(method, bucket, key, { query, headers = {}, body, signal, allow = [] } = {}) {
  const resp = await fetch(url(bucket, key, query), {
    method,
    headers: { ...CSRF, ...headers },
    body,
    signal,
    credentials: 'same-origin',
  });
  if (!resp.ok && !allow.includes(resp.status)) throw await failure(resp);
  return resp;
}

async function xml(method, bucket, key, opts) {
  const resp = await request(method, bucket, key, opts);
  return parse(await resp.text()).documentElement;
}

// ---- buckets ----

export async function listBuckets() {
  const doc = await xml('GET', null, null);
  return kids(kid(doc, 'Buckets'), 'Bucket').map((b) => ({
    name: text(b, 'Name'),
    created: text(b, 'CreationDate'),
  }));
}

export async function createBucket(name, { objectLock = false } = {}) {
  const headers = objectLock ? { 'x-amz-bucket-object-lock-enabled': 'true' } : {};
  await request('PUT', name, null, { headers });
}

export async function deleteBucket(name) {
  await request('DELETE', name, null);
}

export async function getVersioning(bucket) {
  const doc = await xml('GET', bucket, null, { query: { versioning: true } });
  return text(doc, 'Status'); // null | "Enabled" | "Suspended"
}

export async function putVersioning(bucket, status) {
  await request('PUT', bucket, null, { query: { versioning: true }, body: versioningXml(status) });
}

// ---- listing ----

/** One page of a delimiter listing: folders (common prefixes) and objects. */
export async function listObjects(bucket, { prefix = '', delimiter = '/', token, maxKeys = 1000, signal } = {}) {
  const doc = await xml('GET', bucket, null, {
    query: {
      'list-type': '2',
      prefix,
      delimiter,
      'continuation-token': token,
      'max-keys': String(maxKeys),
    },
    signal,
  });
  return {
    prefixes: kids(doc, 'CommonPrefixes').map((p) => text(p, 'Prefix')),
    objects: kids(doc, 'Contents').map((o) => ({
      key: text(o, 'Key'),
      size: int(o, 'Size'),
      lastModified: text(o, 'LastModified'),
      etag: text(o, 'ETag'),
      storageClass: text(o, 'StorageClass'),
    })),
    truncated: text(doc, 'IsTruncated') === 'true',
    nextToken: text(doc, 'NextContinuationToken'),
  };
}

/** Every key under `prefix` (no delimiter), page by page. */
export async function* allKeys(bucket, prefix) {
  let token;
  do {
    const page = await listObjects(bucket, { prefix, delimiter: '', token });
    for (const o of page.objects) yield o;
    token = page.truncated ? page.nextToken : undefined;
  } while (token);
}

/** One page of ListObjectVersions: versions and delete markers in key order. */
export async function listVersions(bucket, { prefix = '', delimiter, keyMarker, versionIdMarker, maxKeys = 1000 } = {}) {
  const doc = await xml('GET', bucket, null, {
    query: {
      versions: true,
      prefix,
      delimiter,
      'key-marker': keyMarker,
      'version-id-marker': versionIdMarker,
      'max-keys': String(maxKeys),
    },
  });
  const entries = Array.from(doc.children)
    .filter((c) => c.localName === 'Version' || c.localName === 'DeleteMarker')
    .map((v) => ({
      key: text(v, 'Key'),
      versionId: text(v, 'VersionId'),
      isLatest: text(v, 'IsLatest') === 'true',
      lastModified: text(v, 'LastModified'),
      size: int(v, 'Size'),
      etag: text(v, 'ETag'),
      storageClass: text(v, 'StorageClass'),
      deleteMarker: v.localName === 'DeleteMarker',
    }));
  return {
    entries,
    prefixes: kids(doc, 'CommonPrefixes').map((p) => text(p, 'Prefix')),
    truncated: text(doc, 'IsTruncated') === 'true',
    nextKeyMarker: text(doc, 'NextKeyMarker'),
    nextVersionIdMarker: text(doc, 'NextVersionIdMarker'),
  };
}

// ---- objects ----

export async function headObject(bucket, key, versionId) {
  const resp = await request('HEAD', bucket, key, {
    query: { versionId },
    headers: { 'x-amz-checksum-mode': 'ENABLED' },
  });
  const h = resp.headers;
  const meta = {};
  const checksums = {};
  h.forEach((v, k) => {
    if (k.startsWith('x-amz-meta-')) meta[k.slice(11)] = v;
    if (k.startsWith('x-amz-checksum-') && k !== 'x-amz-checksum-type') checksums[k.slice(15).toUpperCase()] = v;
  });
  return {
    size: Number(h.get('content-length') ?? 0),
    contentType: h.get('content-type'),
    etag: h.get('etag'),
    lastModified: h.get('last-modified'),
    storageClass: h.get('x-amz-storage-class') ?? 'STANDARD',
    versionId: h.get('x-amz-version-id'),
    tagCount: Number(h.get('x-amz-tagging-count') ?? 0),
    lockMode: h.get('x-amz-object-lock-mode'),
    retainUntil: h.get('x-amz-object-lock-retain-until-date'),
    legalHold: h.get('x-amz-object-lock-legal-hold') === 'ON',
    checksumType: h.get('x-amz-checksum-type'),
    checksums,
    meta,
  };
}

/** A same-origin URL that downloads the object under its own name. */
export function downloadUrl(bucket, key, versionId) {
  const name = key.split('/').pop() || 'download';
  return url(bucket, key, { versionId, 'response-content-disposition': attachment(name) });
}

export async function deleteObject(bucket, key, { versionId, bypassGovernance = false } = {}) {
  const headers = bypassGovernance ? { 'x-amz-bypass-governance-retention': 'true' } : {};
  const resp = await request('DELETE', bucket, key, { query: { versionId }, headers });
  return { versionId: resp.headers.get('x-amz-version-id'), deleteMarker: resp.headers.get('x-amz-delete-marker') === 'true' };
}

/** Deletes up to 1000 objects per request. Returns the failures `[{key, versionId, code, message}]`. */
export async function deleteObjects(bucket, objects, { bypassGovernance = false } = {}) {
  const errors = [];
  for (let i = 0; i < objects.length; i += 1000) {
    const batch = objects.slice(i, i + 1000);
    const headers = { 'content-type': 'application/xml' };
    if (bypassGovernance) headers['x-amz-bypass-governance-retention'] = 'true';
    const doc = await xml('POST', bucket, null, { query: { delete: true }, headers, body: deleteObjectsXml(batch) });
    for (const e of kids(doc, 'Error')) {
      errors.push({ key: text(e, 'Key'), versionId: text(e, 'VersionId'), code: text(e, 'Code'), message: text(e, 'Message') });
    }
  }
  return errors;
}

/** Copies (or, onto the same key, restores) an object version. */
export async function copyObject(bucket, key, source, { metadataDirective } = {}) {
  let src = `/${encodeURIComponent(source.bucket)}/${encodeKey(source.key)}`;
  if (source.versionId) src += `?versionId=${encodeURIComponent(source.versionId)}`;
  const headers = { 'x-amz-copy-source': src };
  if (metadataDirective) headers['x-amz-metadata-directive'] = metadataDirective;
  await request('PUT', bucket, key, { headers });
}

/** An empty `prefix/` object, shown as a folder. */
export async function createFolder(bucket, prefix) {
  await request('PUT', bucket, prefix.endsWith('/') ? prefix : `${prefix}/`, {
    headers: { 'content-type': 'application/x-directory' },
    body: new Blob([]),
  });
}

export async function getTags(bucket, key, versionId) {
  const doc = await xml('GET', bucket, key, { query: { tagging: true, versionId } });
  return kids(kid(doc, 'TagSet'), 'Tag').map((t) => ({ key: text(t, 'Key'), value: text(t, 'Value') ?? '' }));
}

export async function putTags(bucket, key, versionId, tags) {
  if (tags.length === 0) {
    await request('DELETE', bucket, key, { query: { tagging: true, versionId } });
    return;
  }
  await request('PUT', bucket, key, { query: { tagging: true, versionId }, body: taggingXml(tags) });
}

export async function putRetention(bucket, key, versionId, mode, until, { bypassGovernance = false } = {}) {
  const headers = bypassGovernance ? { 'x-amz-bypass-governance-retention': 'true' } : {};
  await request('PUT', bucket, key, { query: { retention: true, versionId }, headers, body: retentionXml(mode, until) });
}

export async function putLegalHold(bucket, key, versionId, on) {
  await request('PUT', bucket, key, { query: { 'legal-hold': true, versionId }, body: legalHoldXml(on) });
}

// ---- uploads ----

/** PUT with upload progress (fetch cannot report it). Resolves to the response headers. */
function xhrPut(target, body, headers, { onProgress, signal } = {}) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', target);
    for (const [k, v] of Object.entries({ ...CSRF, ...headers })) xhr.setRequestHeader(k, v);
    xhr.upload.onprogress = (e) => onProgress?.(e.loaded);
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve({ etag: xhr.getResponseHeader('etag'), versionId: xhr.getResponseHeader('x-amz-version-id') });
      } else {
        if (xhr.status === 401) sessionExpired();
        const doc = xhr.responseText?.startsWith('<') ? parse(xhr.responseText) : null;
        reject(new S3Error(xhr.status, doc?.querySelector('Code')?.textContent, doc?.querySelector('Message')?.textContent));
      }
    };
    xhr.onerror = () => reject(new Error('通信エラーが発生しました'));
    xhr.onabort = () => reject(new DOMException('中止しました', 'AbortError'));
    signal?.addEventListener('abort', () => xhr.abort(), { once: true });
    xhr.send(body);
  });
}

/** Files at or above this size are sent in parts. */
export const MULTIPART_THRESHOLD = 64 * 1024 * 1024;
const PART_CONCURRENCY = 3;

/**
 * Uploads a File/Blob to `key`. `onProgress(bytesSent)`; abort with `signal`.
 * Large files use a multipart upload (aborted on failure or cancel).
 */
export async function upload(bucket, key, file, { contentType, storageClass, onProgress, signal } = {}) {
  const headers = { 'content-type': contentType || file.type || 'application/octet-stream' };
  if (storageClass) headers['x-amz-storage-class'] = storageClass;
  if (file.size < MULTIPART_THRESHOLD) {
    return xhrPut(url(bucket, key), file, headers, { onProgress, signal });
  }
  const created = await xml('POST', bucket, key, { query: { uploads: true }, headers, signal });
  const uploadId = text(created, 'UploadId');
  try {
    const size = partSize(file.size);
    const count = Math.ceil(file.size / size);
    const sent = new Array(count).fill(0);
    const parts = new Array(count);
    let next = 0;
    const worker = async () => {
      while (next < count) {
        const i = next++;
        const blob = file.slice(i * size, Math.min(file.size, (i + 1) * size));
        const r = await xhrPut(url(bucket, key, { partNumber: String(i + 1), uploadId }), blob, {}, {
          signal,
          onProgress: (n) => {
            sent[i] = n;
            onProgress?.(sent.reduce((a, b) => a + b, 0));
          },
        });
        sent[i] = blob.size;
        parts[i] = { partNumber: i + 1, etag: r.etag };
      }
    };
    await Promise.all(Array.from({ length: Math.min(PART_CONCURRENCY, count) }, worker));
    const done = await request('POST', bucket, key, {
      query: { uploadId },
      headers: { 'content-type': 'application/xml' },
      body: completeMultipartXml(parts),
      signal,
    });
    // CompleteMultipartUpload can report an error inside a 200 response.
    const doc = parse(await done.text()).documentElement;
    if (doc.localName === 'Error') throw new S3Error(500, text(doc, 'Code'), text(doc, 'Message'));
    return { etag: text(doc, 'ETag'), versionId: done.headers.get('x-amz-version-id') };
  } catch (e) {
    await request('DELETE', bucket, key, { query: { uploadId }, allow: [404] }).catch(() => {});
    throw e;
  }
}

/** Unfinished multipart uploads of a bucket. */
export async function listUploads(bucket, { prefix = '' } = {}) {
  const doc = await xml('GET', bucket, null, { query: { uploads: true, prefix } });
  return kids(doc, 'Upload').map((u) => ({
    key: text(u, 'Key'),
    uploadId: text(u, 'UploadId'),
    initiated: text(u, 'Initiated'),
    storageClass: text(u, 'StorageClass'),
  }));
}

export async function abortUpload(bucket, key, uploadId) {
  await request('DELETE', bucket, key, { query: { uploadId } });
}

// ---- bucket configuration ----

function numOrNull(v) {
  return v === null ? null : Number(v);
}

export async function getLifecycle(bucket) {
  const resp = await request('GET', bucket, null, { query: { lifecycle: true }, allow: [404] });
  if (resp.status === 404) {
    const e = await failure(resp);
    if (e.code === 'NoSuchLifecycleConfiguration') return [];
    throw e;
  }
  const doc = parse(await resp.text()).documentElement;
  return kids(doc, 'Rule').map((r) => {
    const filter = kid(r, 'Filter');
    const and = kid(filter, 'And');
    const f = and ?? filter;
    const exp = kid(r, 'Expiration');
    const nce = kid(r, 'NoncurrentVersionExpiration');
    return {
      id: text(r, 'ID') ?? '',
      enabled: text(r, 'Status') === 'Enabled',
      prefix: text(f, 'Prefix') ?? text(r, 'Prefix') ?? '',
      tags: kids(f, 'Tag').map((t) => ({ key: text(t, 'Key'), value: text(t, 'Value') ?? '' })),
      sizeGt: numOrNull(text(f, 'ObjectSizeGreaterThan')),
      sizeLt: numOrNull(text(f, 'ObjectSizeLessThan')),
      expirationDays: numOrNull(text(exp, 'Days')),
      expirationDate: text(exp, 'Date')?.slice(0, 10) ?? null,
      expiredObjectDeleteMarker: text(exp, 'ExpiredObjectDeleteMarker') === 'true',
      noncurrentDays: numOrNull(text(nce, 'NoncurrentDays')),
      newerNoncurrentVersions: numOrNull(text(nce, 'NewerNoncurrentVersions')),
      transitions: kids(r, 'Transition').map((t) => ({
        days: numOrNull(text(t, 'Days')),
        date: text(t, 'Date')?.slice(0, 10) ?? null,
        storageClass: text(t, 'StorageClass'),
      })),
      noncurrentTransitions: kids(r, 'NoncurrentVersionTransition').map((t) => ({
        days: numOrNull(text(t, 'NoncurrentDays')),
        storageClass: text(t, 'StorageClass'),
      })),
      abortDays: numOrNull(text(kid(r, 'AbortIncompleteMultipartUpload'), 'DaysAfterInitiation')),
    };
  });
}

export async function putLifecycle(bucket, rules) {
  if (rules.length === 0) {
    await request('DELETE', bucket, null, { query: { lifecycle: true } });
    return;
  }
  await request('PUT', bucket, null, {
    query: { lifecycle: true },
    headers: { 'content-type': 'application/xml' },
    body: lifecycleXml(rules),
  });
}

export async function getCors(bucket) {
  const resp = await request('GET', bucket, null, { query: { cors: true }, allow: [404] });
  if (resp.status === 404) {
    const e = await failure(resp);
    if (e.code === 'NoSuchCORSConfiguration') return [];
    throw e;
  }
  const doc = parse(await resp.text()).documentElement;
  return kids(doc, 'CORSRule').map((r) => ({
    id: text(r, 'ID') ?? '',
    origins: kids(r, 'AllowedOrigin').map((e) => e.textContent),
    methods: kids(r, 'AllowedMethod').map((e) => e.textContent),
    headers: kids(r, 'AllowedHeader').map((e) => e.textContent),
    expose: kids(r, 'ExposeHeader').map((e) => e.textContent),
    maxAge: numOrNull(text(r, 'MaxAgeSeconds')),
  }));
}

export async function putCors(bucket, rules) {
  if (rules.length === 0) {
    await request('DELETE', bucket, null, { query: { cors: true } });
    return;
  }
  await request('PUT', bucket, null, {
    query: { cors: true },
    headers: { 'content-type': 'application/xml' },
    body: corsXml(rules),
  });
}

/** `null` when the bucket has no Object Lock; else `{ mode, days, years }` of the default. */
export async function getObjectLock(bucket) {
  const resp = await request('GET', bucket, null, { query: { 'object-lock': true }, allow: [404] });
  if (resp.status === 404) {
    const e = await failure(resp);
    if (e.code === 'ObjectLockConfigurationNotFoundError') return null;
    throw e;
  }
  const doc = parse(await resp.text()).documentElement;
  if (text(doc, 'ObjectLockEnabled') !== 'Enabled') return null;
  const d = kid(kid(doc, 'Rule'), 'DefaultRetention');
  return { mode: text(d, 'Mode'), days: numOrNull(text(d, 'Days')), years: numOrNull(text(d, 'Years')) };
}

export async function putObjectLock(bucket, config) {
  await request('PUT', bucket, null, {
    query: { 'object-lock': true },
    headers: { 'content-type': 'application/xml' },
    body: objectLockXml(config),
  });
}
