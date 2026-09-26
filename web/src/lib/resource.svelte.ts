import { ApiError, errorMessage } from './api/client';

/**
 * Async data holder for views. `load()` reads its reactive inputs synchronously inside the
 * fetcher, so calling it from `$effect` re-runs when those inputs change. Stale responses
 * from superseded loads are dropped.
 */
export class Resource<T> {
  data: T | undefined = $state.raw(undefined);
  error: string | null = $state(null);
  /** The daemon answered 501: the endpoint ships with a later phase. `error` stays null. */
  unavailable = $state(false);
  loading = $state(false);
  #token = 0;
  readonly #fetcher: () => Promise<T>;

  constructor(fetcher: () => Promise<T>) {
    this.#fetcher = fetcher;
  }

  async load(opts: { keep?: boolean } = {}): Promise<void> {
    const token = ++this.#token;
    this.loading = true;
    this.error = null;
    this.unavailable = false;
    if (!opts.keep) this.data = undefined;
    try {
      const data = await this.#fetcher();
      if (token === this.#token) this.data = data;
    } catch (e) {
      if (token !== this.#token) return;
      if (e instanceof ApiError && e.notImplemented) this.unavailable = true;
      else this.error = errorMessage(e);
    } finally {
      if (token === this.#token) this.loading = false;
    }
  }

  /** Background refresh that keeps the current data on screen. */
  reload(): Promise<void> {
    return this.load({ keep: true });
  }
}
