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
}

export function e2eEnv(): E2eEnv {
  const raw = process.env.BLIRP_E2E;
  if (!raw) throw new Error('BLIRP_E2E is not set; run the suite with `pnpm -C web e2e`');
  return JSON.parse(raw) as E2eEnv;
}
