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
      brief_md: '# B',
    });
    expect(s).toEqual({
      title: 'T',
      summary: 'S',
      decisions: [{ title: 'd', body: 'b' }],
      open_threads: [],
      resolved_record_ids: ['r1'],
      gotchas: [{ title: 'g', body: '' }],
      files: ['a.ts'],
      brief_md: '# B',
    });
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
