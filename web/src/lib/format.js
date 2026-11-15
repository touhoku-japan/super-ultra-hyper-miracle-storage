// Display helpers (Japanese locale).

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

/** 1536 -> "1.5 KiB" */
export function bytes(n) {
  if (n === null || n === undefined || Number.isNaN(Number(n))) return '—';
  let v = Number(n);
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = i === 0 || v >= 100 ? 0 : v >= 10 ? 1 : 2;
  // "64.0 MiB" -> "64 MiB"
  const text = v.toFixed(digits).replace(/\.0+$/, '');
  return `${text} ${UNITS[i]}`;
}

const dtf = new Intl.DateTimeFormat('ja-JP', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
});

/** Unix ms, ISO string or Date -> "2026/11/15 12:34" */
export function datetime(v) {
  if (v === null || v === undefined || v === '') return '—';
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? '—' : dtf.format(d);
}

const nf = new Intl.NumberFormat('ja-JP');
export function count(n) {
  return n === null || n === undefined ? '—' : nf.format(n);
}

/** "12 分前" style age of a unix-ms time. */
export function ago(ms) {
  if (!ms) return '—';
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 時間前`;
  return `${Math.floor(s / 86400)} 日前`;
}

/** Milliseconds as "0.4 秒" / "12 秒" / "3 分 5 秒" / "2 時間 10 分". */
export function duration(ms) {
  if (ms === null || ms === undefined || ms < 0) return '—';
  const s = ms / 1000;
  if (s < 10) return `${Math.round(s * 10) / 10} 秒`;
  if (s < 60) return `${Math.floor(s)} 秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分 ${Math.floor(s % 60)} 秒`;
  return `${Math.floor(m / 60)} 時間 ${m % 60} 分`;
}

/** Parses "10 GiB" / "500MB" / "1.5T" / "1024" into bytes; null for empty, NaN for invalid. */
export function parseBytes(s) {
  const t = String(s ?? '').trim();
  if (!t) return null;
  const m = /^(\d+(?:\.\d+)?)\s*([kmgtp]?)(i?b?)$/i.exec(t);
  if (!m) return NaN;
  const pow = { '': 0, k: 1, m: 2, g: 3, t: 4, p: 5 }[m[2].toLowerCase()];
  return Math.round(Number(m[1]) * 1024 ** pow);
}

/** The last path segment of a key ("a/b/c.txt" -> "c.txt", "a/b/" -> "b/"). */
export function basename(key) {
  const trimmed = key.endsWith('/') ? key.slice(0, -1) : key;
  const i = trimmed.lastIndexOf('/');
  return key.slice(i + 1);
}
