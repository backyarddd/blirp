import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, api, buildUrl, onUnauthorized, request, wsUrl } from './client';

function mockFetch(impl: (url: string, init: RequestInit) => Promise<Response>): ReturnType<typeof vi.fn> {
  const fn = vi.fn(impl);
  vi.stubGlobal('fetch', fn);
  return fn;
}

afterEach(() => {
  vi.unstubAllGlobals();
  onUnauthorized(() => {});
});

describe('buildUrl', () => {
  it('drops empty values and encodes', () => {
    expect(buildUrl('/api/x', { a: 'b c', n: 0, e: '', u: undefined, z: null, t: true })).toBe('/api/x?a=b+c&n=0&t=true');
    expect(buildUrl('/api/x', {})).toBe('/api/x');
  });
});

describe('request', () => {
  it('parses JSON and sends JSON bodies', async () => {
    const f = mockFetch(async () => new Response('{"ok":1}', { status: 200 }));
    await expect(request('POST', '/api/p', { a: 1 })).resolves.toEqual({ ok: 1 });
    const init = f.mock.calls[0]?.[1] as RequestInit;
    expect(init.body).toBe('{"a":1}');
    expect(init.credentials).toBe('same-origin');
    expect((init.headers as Record<string, string>)['Content-Type']).toBe('application/json');
  });

  it('returns undefined on 204', async () => {
    mockFetch(async () => new Response(null, { status: 204 }));
    await expect(request('DELETE', '/api/p/1')).resolves.toBeUndefined();
  });

  it('maps the daemon error envelope', async () => {
    mockFetch(async () => new Response('{"error":{"code":"invalid_path","message":"No such folder"}}', { status: 400 }));
    const err = await request('POST', '/api/projects', {}).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ status: 400, code: 'invalid_path', message: 'No such folder' });
  });

  it('falls back to raw text for non-JSON errors', async () => {
    mockFetch(async () => new Response('Bad Gateway', { status: 502 }));
    await expect(request('GET', '/api/health')).rejects.toMatchObject({ status: 502, code: 'http_502', message: 'Bad Gateway' });
  });

  it('reports network failures with status 0', async () => {
    mockFetch(async () => {
      throw new TypeError('Failed to fetch');
    });
    await expect(request('GET', '/api/health')).rejects.toMatchObject({ status: 0, code: 'network' });
  });

  it('rejects invalid JSON bodies', async () => {
    mockFetch(async () => new Response('{nope', { status: 200 }));
    await expect(request('GET', '/api/health')).rejects.toMatchObject({ code: 'bad_json' });
  });

  it('invokes the unauthorized handler on 401', async () => {
    const handler = vi.fn();
    onUnauthorized(handler);
    mockFetch(async () => new Response('{"error":{"code":"unauthorized","message":"login required"}}', { status: 401 }));
    const err = await request('GET', '/api/projects').catch((e: unknown) => e);
    expect(handler).toHaveBeenCalledOnce();
    expect(err instanceof ApiError && err.unauthorized).toBe(true);
  });

  it('builds endpoint paths with encoded ids and query', async () => {
    const f = mockFetch(async () => new Response('{"path":"a b","diff":""}', { status: 200 }));
    await api.projects.gitDiff('p/1', 'src/a b.ts');
    expect(f.mock.calls[0]?.[0]).toBe('/api/projects/p%2F1/git/diff?path=src%2Fa+b.ts');
    await api.projects.files('p', '', { root: 'C:\\w\\app' });
    expect(f.mock.calls[1]?.[0]).toBe('/api/projects/p/files?root=C%3A%5Cw%5Capp');
  });

  it('flags endpoints of later phases as not implemented', async () => {
    mockFetch(async () => new Response('{"error":{"code":"not_implemented","message":"later"}}', { status: 501 }));
    const err = await api.inject('s').catch((e: unknown) => e);
    expect(err instanceof ApiError && err.notImplemented).toBe(true);
  });
});

describe('wsUrl', () => {
  it('follows the page protocol', () => {
    expect(wsUrl('/api/events/ws', { protocol: 'http:', host: '127.0.0.1:47770' })).toBe('ws://127.0.0.1:47770/api/events/ws');
    expect(wsUrl('/x', { protocol: 'https:', host: 'hub.local:47771' })).toBe('wss://hub.local:47771/x');
  });
});
