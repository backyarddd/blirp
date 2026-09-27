// Typed views of the free-form JSON the daemon stores for memory: `Session.summary`
// (distill output, §9) and `Suggestion.proposal`. Both arrive as parsed JSON of unknown
// shape, so read them defensively: a malformed field degrades to empty, never a crash.
import type { DistillFailure, JsonValue, SessionSummary, Suggestion, SummarizerPick, SummaryItem } from './api/types.gen';
import { agentLabel } from './status';

type JsonObject = { [key in string]: JsonValue };

function isObject(v: JsonValue | undefined | null): v is JsonObject {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

const str = (o: JsonObject, k: string): string => {
  const v = o[k];
  return typeof v === 'string' ? v : '';
};

const optStr = (o: JsonObject, k: string): string | null => {
  const v = o[k];
  return typeof v === 'string' && v !== '' ? v : null;
};

const optNum = (o: JsonObject, k: string): number | null => {
  const v = o[k];
  return typeof v === 'number' ? v : null;
};

const strings = (o: JsonObject, k: string): string[] => {
  const v = o[k];
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
};

const notes = (o: JsonObject, k: string): SummaryItem[] => {
  const v = o[k];
  if (!Array.isArray(v)) return [];
  return v.filter(isObject).map((n) => ({ title: str(n, 'title'), body: str(n, 'body') }));
};

function failure(v: JsonValue | undefined): DistillFailure | null {
  if (!isObject(v)) return null;
  return { message: str(v, 'message') || 'Unknown error', at: optNum(v, 'at') ?? 0, through_seq: optNum(v, 'through_seq') ?? 0 };
}

/** The distill summary of a session, or null when it has none (or it is not an object). */
export function parseSummary(v: JsonValue | null | undefined): SessionSummary | null {
  if (!isObject(v)) return null;
  const o = v;
  return {
    title: optStr(o, 'title'),
    summary: optStr(o, 'summary'),
    decisions: notes(o, 'decisions'),
    open_threads: notes(o, 'open_threads'),
    gotchas: notes(o, 'gotchas'),
    resolved_record_ids: strings(o, 'resolved_record_ids'),
    files: strings(o, 'files'),
    backend: optStr(o, 'backend'),
    distilled_at: optNum(o, 'distilled_at'),
    through_seq: optNum(o, 'through_seq') ?? 0,
    error: failure(o['error']),
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

/** One line on what the `auto` summarizer uses now and why (Settings > Memory). */
export function describeAutoSummarizer(p: SummarizerPick): string {
  const agent = agentLabel(p.default_agent);
  if (!p.backend) {
    return 'Automatic finds no summarizer: install and sign in to Claude Code (or Codex as your default agent), or start Ollama.';
  }
  // claude's model is an alias (`sonnet`): shown as a name. Ollama tags stay as typed.
  const model = p.backend === 'claude' && p.model ? p.model.charAt(0).toUpperCase() + p.model.slice(1) : p.model;
  const name = `${p.backend === 'ollama' ? 'Ollama' : agentLabel(p.backend)} (${model ?? 'its default model'})`;
  switch (p.fallback) {
    case null:
      return `Automatic uses ${name}, your default agent.`;
    case 'no_backend':
      return `Automatic uses ${name}: your default agent, ${agent}, has no summarizer.`;
    case 'not_installed':
      return `Automatic uses ${name}: your default agent, ${agent}, is not installed.`;
    case 'not_logged_in':
      return p.backend === p.default_agent
        ? `Automatic uses ${name}, your default agent, but it is not signed in: distilling pauses until you sign in.`
        : `Automatic uses ${name}: your default agent, ${agent}, is not signed in.`;
  }
}
