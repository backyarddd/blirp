// Starts the real blirp daemon on a throwaway BLIRP_HOME for the end-to-end suite and
// prepares two projects on disk: a plain folder (no git) and a git repository with one
// modified and one untracked file. The daemon's update check asks a local fake releases API
// that offers FAKE_RELEASE, so the suite never calls GitHub. Connection details reach the tests through
// `process.env.BLIRP_E2E` (Playwright passes the global setup's env to its workers).
import { execFileSync, spawn } from 'node:child_process';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { FAKE_RELEASE, type E2eEnv } from './env';

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
  const userHome = join(root, 'user');
  const plain = join(root, 'Plain Folder');
  const repo = join(root, 'git-repo');
  mkdirSync(home);
  // The tests drop agent transcripts here; ingest watches roots that exist at startup.
  mkdirSync(join(userHome, '.claude', 'projects'), { recursive: true });
  // No summarizer (manual distill records a clear failure) and no relay traffic when the
  // suite turns this machine into a hub.
  // keep_awake: live sessions hold a sleep-prevention assertion, shown in the top bar.
  writeFileSync(
    join(home, 'config.toml'),
    '[sessions]\nkeep_awake = true\n\n[memory]\nsummarizer = "none"\n\n[sync]\nrelay = "disabled"\n',
  );
  // Folders the new-session folder picker lists in the (temp) user home.
  mkdirSync(join(userHome, 'code', 'demo-repo', '.git'), { recursive: true });
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

  // GitHub's `releases/latest` for a release newer than any build. It has no assets: the
  // suite never runs an update.
  const releases = createServer((req, res) => {
    if (req.method === 'GET' && req.url === '/releases/latest') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({ tag_name: `v${FAKE_RELEASE}`, html_url: `https://example.invalid/releases/v${FAKE_RELEASE}`, assets: [] }),
      );
    } else {
      res.writeHead(404).end();
    }
  });
  await new Promise<void>((r) => releases.listen(0, '127.0.0.1', r));
  const releasesUrl = `http://127.0.0.1:${(releases.address() as AddressInfo).port}/releases`;

  const log = createWriteStream(join(root, 'daemon.log'));
  // A login token in the developer's own environment would override the one the suite stores.
  const { CLAUDE_CODE_OAUTH_TOKEN: _ownToken, ...parentEnv } = process.env;
  const daemon = spawn(bin, ['daemon', '--port', '0'], {
    cwd: repoRoot,
    env: {
      ...parentEnv,
      BLIRP_HOME: home,
      HOME: userHome,
      USERPROFILE: userHome,
      CLAUDE_CONFIG_DIR: join(userHome, '.claude'),
      CODEX_HOME: join(userHome, '.codex'),
      XDG_CONFIG_HOME: join(userHome, '.config'),
      XDG_DATA_HOME: join(userHome, '.local', 'share'),
      APPDATA: join(userHome, 'AppData', 'Roaming'),
      RUST_LOG: process.env.RUST_LOG ?? 'info',
      // Loopback-only sockets, no relays or mDNS: no firewall prompt for this debug binary.
      BLIRP_LOOPBACK_ONLY: '1',
      BLIRP_RELEASE_BASE_URL: releasesUrl,
    },
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

  const env: E2eEnv = { url, token: runtime.token, plain, repo, root, userHome };
  process.env.BLIRP_E2E = JSON.stringify(env);

  return async () => {
    if (!exited) {
      const done = new Promise((r) => daemon.once('exit', r));
      daemon.kill();
      await done;
    }
    await new Promise<void>((r) => log.end(r));
    releases.closeAllConnections();
    await new Promise<void>((r) => releases.close(() => r()));
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
