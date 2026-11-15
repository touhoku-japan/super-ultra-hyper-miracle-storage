// Starts a real `suhms serve` for the browser tests: a temporary directory with three disks,
// SQLite metadata, and the S3, admin and customer console listeners on free local ports.
// The binary must be built after `npm run build` so it embeds the current console bundle.

import { spawn, execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync, createWriteStream } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const bin = process.env.SUHMS_BIN ?? resolve(here, '../../target/debug/suhms');

function freePort() {
  return new Promise((ok, fail) => {
    const s = createServer();
    s.unref();
    s.on('error', fail);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => ok(port));
    });
  });
}

async function waitFor(url, child, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`suhms exited with ${child.exitCode}`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      // not listening yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`suhms did not answer ${url}`);
}

export default async function globalSetup() {
  const dir = mkdtempSync(join(tmpdir(), 'suhms-e2e-'));
  const disks = ['d1', 'd2', 'd3'].map((d) => join(dir, d));
  for (const d of disks) mkdirSync(d);
  const [s3, admin, console] = await Promise.all([freePort(), freePort(), freePort()]);

  const config = join(dir, 'config.toml');
  writeFileSync(
    config,
    `[server]
listen = "127.0.0.1:${s3}"
[storage]
disks = ${JSON.stringify(disks)}
metadata_url = "sqlite://${join(dir, 'meta.db')}"
default_replicas = 2
[admin]
listen = "127.0.0.1:${admin}"
[console]
listen = "127.0.0.1:${console}"
s3_public_url = "http://127.0.0.1:${s3}"
secure_cookie = false
`,
  );

  const masterKey = execFileSync(bin, ['gen-master-key'], { encoding: 'utf8' }).trim();
  const token = randomBytes(24).toString('hex');
  const log = createWriteStream(join(dir, 'server.log'));
  const child = spawn(bin, ['-c', config, 'serve'], {
    env: { ...process.env, SUHMS_MASTER_KEY: masterKey, SUHMS_ADMIN_TOKEN: token, RUST_LOG: 'warn' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.pipe(log);
  child.stderr.pipe(log);

  try {
    await waitFor(`http://127.0.0.1:${s3}/super-ultra-hyper-miracle-storage/health`, child);
  } catch (e) {
    child.kill();
    throw new Error(`${e.message} (log: ${join(dir, 'server.log')})`);
  }

  // Read by the tests (workers inherit the environment of the runner).
  process.env.SUHMS_E2E_S3 = `http://127.0.0.1:${s3}`;
  process.env.SUHMS_E2E_ADMIN = `http://127.0.0.1:${admin}`;
  process.env.SUHMS_E2E_CONSOLE = `http://127.0.0.1:${console}`;
  process.env.SUHMS_E2E_TOKEN = token;

  return async () => {
    child.kill('SIGTERM');
    await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
    if (process.env.SUHMS_E2E_KEEP) process.stdout.write(`kept ${dir}\n`);
    else rmSync(dir, { recursive: true, force: true });
  };
}
