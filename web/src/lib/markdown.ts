import { marked } from 'marked';
import DOMPurify from 'dompurify';

let hooked = false;

function ensureHooks(): void {
  if (hooked) return;
  hooked = true;
  // Links from agent-written markdown open outside the app and never get the opener.
  DOMPurify.addHook('afterSanitizeAttributes', (node) => {
    if (node.tagName === 'A' && node.hasAttribute('href')) {
      node.setAttribute('target', '_blank');
      node.setAttribute('rel', 'noopener noreferrer');
    }
  });
}

/** Markdown -> sanitized HTML. Safe to pass to {@html}. */
export function renderMarkdown(src: string): string {
  ensureHooks();
  const html = marked.parse(src, { async: false, gfm: true, breaks: false });
  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    FORBID_TAGS: ['style', 'form', 'input', 'button', 'textarea', 'select'],
    FORBID_ATTR: ['style'],
  });
}

export interface SnippetPart {
  text: string;
  match: boolean;
}

/** Splits an FTS snippet whose matches are wrapped in U+0002 ... U+0003. Rendered as text, never HTML. */
export function splitSnippet(snippet: string): SnippetPart[] {
  const parts: SnippetPart[] = [];
  let match = false;
  let buf = '';
  for (const ch of snippet) {
    if (ch === '\u0002' || ch === '\u0003') {
      if (buf) parts.push({ text: buf, match });
      buf = '';
      match = ch === '\u0002';
    } else {
      buf += ch;
    }
  }
  if (buf) parts.push({ text: buf, match });
  return parts;
}

/** URL-safe wiki slug: lowercase ASCII words joined by dashes. */
export function slugify(title: string): string {
  return title
    .normalize('NFKD')
    .replace(/\p{M}/gu, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 80);
}
