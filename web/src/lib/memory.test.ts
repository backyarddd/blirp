import { describe, expect, it } from 'vitest';
import { describeAutoSummarizer, parseSummary, proposalMarkdown } from './memory';

describe('parseSummary', () => {
  it('reads the distill contract', () => {
    const s = parseSummary({
      title: 'T',
      summary: 'S',
      decisions: [{ title: 'd', body: 'b' }],
      open_threads: [],
      resolved_record_ids: ['r1'],
      gotchas: [{ title: 'g' }],
      files: ['a.ts', 3],
      backend: 'claude',
      distilled_at: 5,
      through_seq: 9,
      error: null,
    });
    expect(s).toEqual({
      title: 'T',
      summary: 'S',
      decisions: [{ title: 'd', body: 'b' }],
      open_threads: [],
      gotchas: [{ title: 'g', body: '' }],
      resolved_record_ids: ['r1'],
      files: ['a.ts'],
      backend: 'claude',
      distilled_at: 5,
      through_seq: 9,
      error: null,
    });
  });
  it('reads a failed attempt without a summary', () => {
    const s = parseSummary({ summary: null, error: { message: 'no backend', at: 7, through_seq: 3 } });
    expect(s?.summary).toBeNull();
    expect(s?.error).toEqual({ message: 'no backend', at: 7, through_seq: 3 });
  });
  it('rejects non-objects and tolerates missing fields', () => {
    expect(parseSummary(null)).toBeNull();
    expect(parseSummary('text')).toBeNull();
    expect(parseSummary([1])).toBeNull();
    expect(parseSummary({ summary: 'only' })?.decisions).toEqual([]);
  });
});

describe('proposalMarkdown', () => {
  it('renders each target', () => {
    expect(proposalMarkdown({ target: 'brief', proposal: { body_md: 'x' } })).toBe('x');
    expect(proposalMarkdown({ target: 'wiki', proposal: { slug: 's', title: 'W', body_md: 'y' } })).toBe('### W\n\ny');
    expect(proposalMarkdown({ target: 'record', proposal: { title: 'R', body: 'z' } })).toBe('**R**\n\nz');
    expect(proposalMarkdown({ target: 'record', proposal: 'bad' })).toBe('');
  });
});

describe('describeAutoSummarizer', () => {
  it('names the backend, its model and why', () => {
    expect(describeAutoSummarizer({ backend: 'claude', model: 'sonnet', default_agent: 'claude', fallback: null })).toBe(
      'Automatic uses Claude Code (Sonnet), your default agent.',
    );
    expect(describeAutoSummarizer({ backend: 'codex', model: null, default_agent: 'codex', fallback: null })).toBe(
      'Automatic uses Codex (its default model), your default agent.',
    );
    expect(describeAutoSummarizer({ backend: 'ollama', model: 'qwen2.5:7b', default_agent: 'codex', fallback: 'not_logged_in' })).toBe(
      'Automatic uses Ollama (qwen2.5:7b): your default agent, Codex, is not signed in.',
    );
    expect(describeAutoSummarizer({ backend: 'claude', model: 'sonnet', default_agent: 'opencode', fallback: 'no_backend' })).toBe(
      'Automatic uses Claude Code (Sonnet): your default agent, opencode, has no summarizer.',
    );
    expect(describeAutoSummarizer({ backend: 'claude', model: 'sonnet', default_agent: 'custom:mine', fallback: 'no_backend' })).toBe(
      'Automatic uses Claude Code (Sonnet): your default agent, mine, has no summarizer.',
    );
    expect(describeAutoSummarizer({ backend: 'ollama', model: 'llama3', default_agent: 'claude', fallback: 'not_installed' })).toBe(
      'Automatic uses Ollama (llama3): your default agent, Claude Code, is not installed.',
    );
    expect(describeAutoSummarizer({ backend: 'claude', model: 'sonnet', default_agent: 'codex', fallback: 'outdated' })).toBe(
      'Automatic uses Claude Code (Sonnet): your default agent, Codex, is too old to summarize (update it).',
    );
    expect(describeAutoSummarizer({ backend: 'claude', model: 'sonnet', default_agent: 'claude', fallback: 'not_logged_in' })).toContain(
      'but it is not signed in',
    );
    expect(describeAutoSummarizer({ backend: null, model: null, default_agent: 'claude', fallback: 'not_installed' })).toContain(
      'finds no summarizer',
    );
  });
});
