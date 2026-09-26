// Typed views of the free-form JSON the daemon stores for memory: `Session.summary`
// (distill output, §9) and `Suggestion.proposal`. Both arrive as parsed JSON of unknown
// shape, so read them defensively: a malformed field degrades to empty, never a crash.
import type { JsonValue, Suggestion } from './api/types.gen';
import type { SessionSummary, TitledNote } from './api/types.pending';

type JsonObject = { [key in string]: JsonValue };

function isObject(v: JsonValue | undefined | null): v is JsonObject {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

const str = (o: JsonObject, k: string): string => {
  const v = o[k];
  return typeof v === 'string' ? v : '';
};

const strings = (o: JsonObject, k: string): string[] => {
  const v = o[k];
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
};

const notes = (o: JsonObject, k: string): TitledNote[] => {
  const v = o[k];
  if (!Array.isArray(v)) return [];
  return v.filter(isObject).map((n) => ({ title: str(n, 'title'), body: str(n, 'body') }));
};

/** The distill summary of a session, or null when it has none (or it is not an object). */
export function parseSummary(v: JsonValue | null | undefined): SessionSummary | null {
  if (!isObject(v)) return null;
  const o = v;
  return {
    title: str(o, 'title'),
    summary: str(o, 'summary'),
    decisions: notes(o, 'decisions'),
    open_threads: notes(o, 'open_threads'),
    resolved_record_ids: strings(o, 'resolved_record_ids'),
    gotchas: notes(o, 'gotchas'),
    files: strings(o, 'files'),
    brief_md: str(o, 'brief_md'),
  };
}

/** Markdown preview of a suggestion's proposal (`BriefProposal`, `RecordProposal` or `WikiProposal`). */
export function proposalMarkdown(s: Pick<Suggestion, 'target' | 'proposal'>): string {
  const p = s.proposal;
  if (!isObject(p)) return '';
  if (s.target === 'brief') return str(p, 'body_md');
  if (s.target === 'wiki') return `### ${str(p, 'title')}\n\n${str(p, 'body_md')}`;
  return `**${str(p, 'title')}**\n\n${str(p, 'body')}`;
}
