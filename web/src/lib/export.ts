// Exports built in the browser from what the API already serves: a session's transcript (all its
// events, read page by page) and a project's memory (the shape of `blirp mem brief --json`), as
// Markdown or JSON, saved as a file download.
import type { Brief, Event, EventKind, EventsPage, Project, ProjectSummary, Record as MemoryRecord, Session } from './api/types.gen';
import { KIND_LABEL } from './memory';
import { agentLabel, sessionTitle } from './status';

export type ExportFormat = 'markdown' | 'json';

/** Events read per request (the API's maximum). */
export const EVENTS_PAGE = 1000;

/** Every event of a session: `page(after)` is the events API; stops at the last page. */
export async function allEvents(page: (after: number) => Promise<EventsPage>): Promise<Event[]> {
  const out: Event[] = [];
  // Seqs start at 0; a page holds the events after `after`.
  let after = -1;
  for (;;) {
    const p = await page(after);
    out.push(...p.items);
    // A cursor that does not move on would loop forever; the API never sends one.
    if (p.next_after === null || p.next_after <= after) return out;
    after = p.next_after;
  }
}

const iso = (ms: number): string => new Date(ms).toISOString();

/** A fence longer than any run of backticks in `text`, so the text can never close it. */
export function fence(text: string): string {
  const longest = Math.max(0, ...[...text.matchAll(/`+/g)].map((m) => m[0].length));
  return '`'.repeat(Math.max(3, longest + 1));
}

function fenced(text: string, info = ''): string {
  const f = fence(text);
  return `${f}${info}\n${text}\n${f}`;
}

const EVENT_HEADING: Record<EventKind, string> = {
  user: 'User',
  assistant: 'Assistant',
  tool_call: 'Tool call',
  tool_result: 'Tool result',
  system: 'System',
  file_edit: 'File edit',
  summary: 'Summary',
};

/** Prompts and replies are Markdown already; tool traffic and system text are shown verbatim. */
const PROSE: ReadonlySet<EventKind> = new Set(['user', 'assistant', 'summary']);

export function transcriptMarkdown(s: Session, events: readonly Event[], projectName: string | null): string {
  const lines = [
    `# ${sessionTitle(s)}`,
    '',
    `- Agent: ${agentLabel(s.agent)}`,
    ...(projectName ? [`- Project: ${projectName}`] : []),
    `- Folder: ${s.cwd}`,
    `- Started: ${iso(s.started_at)}`,
    `- Ended: ${s.ended_at === null ? 'not ended' : iso(s.ended_at)}`,
    `- Status: ${s.status}`,
    `- Session id: ${s.id}`,
    '',
  ];
  if (events.length === 0) lines.push('_No transcript events._', '');
  for (const e of events) {
    lines.push(`## ${EVENT_HEADING[e.kind]} (${iso(e.ts)})`, '');
    lines.push(PROSE.has(e.kind) ? e.text.trim() || '_(empty)_' : fenced(e.text), '');
  }
  return lines.join('\n');
}

/** `{session, events}`, the shape of `blirp mem show --json`, with every event. */
export function transcriptJson(s: Session, events: readonly Event[]): string {
  return `${JSON.stringify({ session: s, events }, null, 2)}\n`;
}

/** The project row itself, as `blirp mem brief --json` prints it. */
function projectRow(p: ProjectSummary): Project {
  return {
    id: p.id,
    name: p.name,
    created_at: p.created_at,
    updated_at: p.updated_at,
    deleted: p.deleted,
    chats: p.chats,
    merged_into: p.merged_into,
  };
}

/** `{project, brief, records}` (active records, pinned first), the shape of `blirp mem brief --json`. */
export function memoryJson(p: ProjectSummary, brief: Brief | null, records: readonly MemoryRecord[]): string {
  return `${JSON.stringify({ project: projectRow(p), brief, records }, null, 2)}\n`;
}

export function memoryMarkdown(p: ProjectSummary, brief: Brief | null, records: readonly MemoryRecord[], at: number): string {
  const lines = [`# ${p.name}`, '', `- Project id: ${p.id}`, `- Exported: ${iso(at)}`, '', '## Brief', ''];
  lines.push(brief?.body_md.trim() || '_No brief yet._', '');
  lines.push('## Records', '');
  if (records.length === 0) lines.push('_No active records._', '');
  for (const r of records) {
    lines.push(`### ${KIND_LABEL[r.kind]}: ${r.title}${r.pinned ? ' (pinned)' : ''}`, '');
    if (r.body.trim()) lines.push(r.body.trim(), '');
    lines.push(`_${r.updated_by}, ${iso(r.updated_at)}_`, '');
  }
  return lines.join('\n');
}

/** A file name from a title: letters, digits and dashes, never empty. */
export function exportName(title: string, ext: 'md' | 'json'): string {
  const base = title
    .normalize('NFKD')
    .replace(/\p{M}/gu, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 80);
  return `${base || 'export'}.${ext}`;
}

/** Save `text` as a download named `name`. */
export function download(name: string, text: string, type: string): void {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.rel = 'noopener';
  document.body.append(a);
  a.click();
  a.remove();
  // The download has started from the click; the URL only has to outlive it.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}
