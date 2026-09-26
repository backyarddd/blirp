import { describe, expect, it } from 'vitest';
import { href, isRouterClick, matchRoute } from './router';

describe('matchRoute', () => {
  it('maps the root to sessions', () => {
    expect(matchRoute('/')).toEqual({ name: 'sessions', sessionId: null });
  });
  it('matches sessions and decodes ids', () => {
    expect(matchRoute('/sessions/abc%2Fd')).toEqual({ name: 'sessions', sessionId: 'abc/d' });
    expect(matchRoute('/sessions/')).toEqual({ name: 'sessions', sessionId: null });
  });
  it('matches project tabs', () => {
    expect(matchRoute('/projects')).toEqual({ name: 'projects' });
    expect(matchRoute('/projects/p1')).toEqual({ name: 'project', projectId: 'p1', tab: 'overview', sub: null });
    expect(matchRoute('/projects/p1/wiki/setup')).toEqual({ name: 'project', projectId: 'p1', tab: 'wiki', sub: 'setup' });
    expect(matchRoute('/projects/p1/bogus').name).toBe('not_found');
  });
  it('parses search params', () => {
    expect(matchRoute('/search', '?q=worktree&kind=record')).toEqual({ name: 'search', q: 'worktree', project: null, kind: 'record' });
  });
  it('matches settings sections', () => {
    expect(matchRoute('/settings')).toEqual({ name: 'settings', section: 'agents' });
    expect(matchRoute('/settings/sync')).toEqual({ name: 'settings', section: 'sync' });
    expect(matchRoute('/settings/nope').name).toBe('not_found');
  });
  it('round-trips href builders', () => {
    expect(href.project('p 1', 'wiki', 'a/b')).toBe('/projects/p%201/wiki/a%2Fb');
    expect(matchRoute(href.project('p 1', 'wiki', 'a/b'))).toEqual({ name: 'project', projectId: 'p 1', tab: 'wiki', sub: 'a/b' });
    expect(href.project('p1')).toBe('/projects/p1');
    expect(href.search('a b', 'p1')).toBe('/search?q=a+b&project=p1');
  });
});

describe('isRouterClick', () => {
  const ev = { button: 0, metaKey: false, ctrlKey: false, shiftKey: false, altKey: false, defaultPrevented: false };
  const a = (url: string, target = '', attrs: string[] = []) => ({
    href: url,
    target,
    hasAttribute: (n: string) => attrs.includes(n),
  });
  const origin = 'http://127.0.0.1:47770';
  it('handles plain same-origin clicks', () => {
    expect(isRouterClick(ev, a(`${origin}/projects`), origin)).toBe(true);
  });
  it('leaves modified, external, targeted and server paths alone', () => {
    expect(isRouterClick({ ...ev, ctrlKey: true }, a(`${origin}/projects`), origin)).toBe(false);
    expect(isRouterClick(ev, a('https://example.com/'), origin)).toBe(false);
    expect(isRouterClick(ev, a(`${origin}/x`, '_blank'), origin)).toBe(false);
    expect(isRouterClick(ev, a(`${origin}/api/health`), origin)).toBe(false);
    expect(isRouterClick(ev, a(`${origin}/mcp`), origin)).toBe(false);
    expect(isRouterClick(ev, a(`${origin}/x`, '', ['download']), origin)).toBe(false);
  });
});
