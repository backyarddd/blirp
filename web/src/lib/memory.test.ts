import { describe, expect, it } from 'vitest';
import { parseSummary, proposalMarkdown } from './memory';

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
