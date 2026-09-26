// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import { renderMarkdown, slugify, splitSnippet } from './markdown';

describe('renderMarkdown', () => {
  it('renders GFM', () => {
    const html = renderMarkdown('# Brief\n\n- [x] done\n\n`code`');
    expect(html).toContain('<h1>Brief</h1>');
    expect(html).toContain('<code>code</code>');
  });
  it('strips scripts, handlers, javascript: urls and inline styles', () => {
    const html = renderMarkdown(
      '<script>alert(1)</script>\n\n<img src=x onerror="alert(1)">\n\n' +
        '[x](javascript:alert(1)) and <a href="javascript:alert(2)">y</a>\n\n<div style="color:red">s</div>',
    );
    expect(html).not.toMatch(/<script|onerror|javascript:|style=/i);
  });
  it('opens links in a new context without opener', () => {
    const html = renderMarkdown('[docs](https://example.com)');
    expect(html).toContain('target="_blank"');
    expect(html).toContain('rel="noopener noreferrer"');
  });
});

describe('splitSnippet', () => {
  it('splits on FTS markers', () => {
    expect(splitSnippet('a \u0002hit\u0003 b')).toEqual([
      { text: 'a ', match: false },
      { text: 'hit', match: true },
      { text: ' b', match: false },
    ]);
  });
  it('keeps HTML as literal text', () => {
    expect(splitSnippet('<b>\u0002x\u0003')).toEqual([
      { text: '<b>', match: false },
      { text: 'x', match: true },
    ]);
  });
});

describe('slugify', () => {
  it('makes URL-safe slugs', () => {
    expect(slugify('  Déploiement & Setup Notes! ')).toBe('deploiement-setup-notes');
    expect(slugify('***')).toBe('');
  });
});
