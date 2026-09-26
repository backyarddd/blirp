// Starts the real blirp daemon on a throwaway BLIRP_HOME for the end-to-end suite and
// prepares two projects on disk: a plain folder (no git) and a git repository with one
// modified and one untracked file. Connection details reach the tests through
// `process.env.BLIRP_E2E` (Playwright passes the global setup's env to its workers).
import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import type { E2eEnv } from './env';

const repoRoot = resolve(import.meta.dirname, '../..');

function git(cwd: string, ...args: string[]): void {
  execFileSync('git', args, { cwd, stdio: 'pipe' });
}

async function waitFor<T>(what: string, timeoutMs: number, probe: () => Promise<T | undefined>): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const v = await probe();
      if (v !== undefined) return v;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${String(last)}` : ''}`);
}

export default async function globalSetup(): Promise<() => Promise<void>> {
  // The SPA must already be in web/dist (`pnpm e2e` builds it); debug daemons serve it from disk.
  if (!existsSync(join(repoRoot, 'web', 'dist', 'index.html'))) {
    throw new Error('web/dist is missing; run `pnpm -C web build` first (or use `pnpm -C web e2e`)');
  }
  execFileSync('cargo', ['build', '-p', 'blirp'], { cwd: repoRoot, stdio: 'inherit' });
  const targetDir = process.env.CARGO_TARGET_DIR ?? join(repoRoot, 'target');
  const bin = join(targetDir, 'debug', process.platform === 'win32' ? 'blirp.exe' : 'blirp');

  const root = mkdtempSync(join(tmpdir(), 'blirp-e2e-'));
  const home = join(root, 'home');
  const plain = join(root, 'Plain Folder');
  const repo = join(root, 'git-repo');
  mkdirSync(home);
  mkdirSync(join(plain, 'sub'), { recursive: true });
  writeFileSync(join(plain, 'notes.txt'), 'hello from a plain folder\nsecond line\n');
  writeFileSync(join(plain, 'sub', 'inner.txt'), 'nested\n');
  mkdirSync(repo);
  git(repo, 'init', '-q', '-b', 'main');
  git(repo, 'config', 'user.email', 'e2e@blirp.invalid');
  git(repo, 'config', 'user.name', 'blirp e2e');
  git(repo, 'config', 'commit.gpgsign', 'false');
  writeFileSync(join(repo, 'README.md'), '# repo\n\noriginal line\n');
  git(repo, 'add', '.');
  git(repo, 'commit', '-q', '-m', 'init');
  writeFileSync(join(repo, 'README.md'), '# repo\n\nchanged line\n');
  writeFileSync(join(repo, 'new-file.txt'), 'untracked\n');

  const log = createWriteStream(join(root, 'daemon.log'));
  const daemon = spawn(bin, ['daemon', '--port', '0'], {
    cwd: repoRoot,
    env: { ...process.env, BLIRP_HOME: home, RUST_LOG: process.env.RUST_LOG ?? 'info' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  daemon.stdout.pipe(log);
  daemon.stderr.pipe(log);
  let exited = false;
  daemon.on('exit', () => (exited = true));

  const runtime = await waitFor('runtime.json', 60_000, async () => {
    if (exited) throw new Error(`daemon exited early; see ${join(root, 'daemon.log')}`);
    const file = join(home, 'runtime.json');
    if (!existsSync(file)) return undefined;
    return JSON.parse(readFileSync(file, 'utf8')) as { port: number; token: string };
  });
  const url = `http://127.0.0.1:${runtime.port}`;
  await waitFor('/api/health', 30_000, async () => {
    const res = await fetch(`${url}/api/health`, { headers: { Authorization: `Bearer ${runtime.token}` } });
    return res.ok ? true : undefined;
  });

  const env: E2eEnv = { url, token: runtime.token, plain, repo, root };
  process.env.BLIRP_E2E = JSON.stringify(env);

  return async () => {
    if (!exited) {
      const done = new Promise((r) => daemon.once('exit', r));
      daemon.kill();
      await done;
    }
    await new Promise<void>((r) => log.end(r));
    if (process.env.BLIRP_E2E_KEEP) {
      console.log(`blirp e2e: kept ${root}`);
      return;
    }
    try {
      rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch (e) {
      console.warn(`blirp e2e: could not remove ${root}: ${String(e)}`);
    }
  };
}
