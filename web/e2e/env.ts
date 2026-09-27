/** The release the fake releases API offers (newer than any build). */
export const FAKE_RELEASE = '99.0.0';

export interface E2eEnv {
  /** Daemon origin, e.g. `http://127.0.0.1:53211`. */
  url: string;
  token: string;
  /** A project folder without git. */
  plain: string;
  /** A git repository with a modified README.md and an untracked new-file.txt. */
  repo: string;
  /** Temp dir holding everything, including `daemon.log`. */
  root: string;
  /**
   * The daemon's HOME/USERPROFILE (and CLAUDE_CONFIG_DIR, CODEX_HOME, XDG dirs under it):
   * ingest reads and global integration writes only here, never the real user's config.
   */
  userHome: string;
}

export function e2eEnv(): E2eEnv {
  const raw = process.env.BLIRP_E2E;
  if (!raw) throw new Error('BLIRP_E2E is not set; run the suite with `pnpm -C web e2e`');
  return JSON.parse(raw) as E2eEnv;
}
