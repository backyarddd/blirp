// Pure route table. Reactive state lives in router.svelte.ts.

export type ProjectTab = 'overview' | 'sessions' | 'memory' | 'wiki' | 'resources' | 'files' | 'hub-files' | 'git';
export const PROJECT_TABS: readonly ProjectTab[] = ['overview', 'sessions', 'memory', 'wiki', 'resources', 'files', 'hub-files', 'git'];

export type SettingsSection = 'agents' | 'memory' | 'sync' | 'portal' | 'appearance' | 'about';
export const SETTINGS_SECTIONS: readonly SettingsSection[] = ['agents', 'memory', 'sync', 'portal', 'appearance', 'about'];

export type Route =
  | { name: 'sessions'; sessionId: string | null }
  | { name: 'grid' }
  | { name: 'projects' }
  | { name: 'project'; projectId: string; tab: ProjectTab; sub: string | null }
  | { name: 'search'; q: string; project: string | null; kind: string | null }
  | { name: 'settings'; section: SettingsSection }
  | { name: 'not_found'; path: string };

function segments(pathname: string): string[] {
  return pathname
    .split('/')
    .filter(Boolean)
    .map((s) => {
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    });
}

const isTab = (s: string | undefined): s is ProjectTab => PROJECT_TABS.includes(s as ProjectTab);
const isSection = (s: string | undefined): s is SettingsSection => SETTINGS_SECTIONS.includes(s as SettingsSection);

export function matchRoute(pathname: string, search = ''): Route {
  const seg = segments(pathname);
  const params = new URLSearchParams(search);
  const [a, b, c, d] = seg;
  if (a === undefined) return { name: 'sessions', sessionId: null };
  switch (a) {
    case 'sessions':
      if (seg.length <= 2) return { name: 'sessions', sessionId: b ?? null };
      break;
    case 'grid':
      if (seg.length === 1) return { name: 'grid' };
      break;
    case 'projects':
      if (b === undefined) return { name: 'projects' };
      if (c === undefined) return { name: 'project', projectId: b, tab: 'overview', sub: null };
      if (isTab(c) && seg.length <= 4) return { name: 'project', projectId: b, tab: c, sub: d ?? null };
      break;
    case 'search':
      if (seg.length === 1)
        return { name: 'search', q: params.get('q') ?? '', project: params.get('project'), kind: params.get('kind') };
      break;
    case 'settings':
      if (b === undefined) return { name: 'settings', section: 'agents' };
      if (isSection(b) && seg.length === 2) return { name: 'settings', section: b };
      break;
  }
  return { name: 'not_found', path: pathname };
}

const e = encodeURIComponent;

export const href = {
  sessions: (id?: string | null): string => (id ? `/sessions/${e(id)}` : '/sessions'),
  grid: (): string => '/grid',
  projects: (): string => '/projects',
  project: (id: string, tab: ProjectTab = 'overview', sub?: string): string =>
    `/projects/${e(id)}${tab === 'overview' && !sub ? '' : `/${tab}`}${sub ? `/${e(sub)}` : ''}`,
  search: (q = '', project?: string | null, kind?: string | null): string => {
    const p = new URLSearchParams();
    if (q) p.set('q', q);
    if (project) p.set('project', project);
    if (kind) p.set('kind', kind);
    const qs = p.toString();
    return qs ? `/search?${qs}` : '/search';
  },
  settings: (section: SettingsSection = 'agents'): string => `/settings/${section}`,
};

/** Should a click on this anchor be handled by the client router? */
export function isRouterClick(
  ev: Pick<MouseEvent, 'button' | 'metaKey' | 'ctrlKey' | 'shiftKey' | 'altKey' | 'defaultPrevented'>,
  anchor: { href: string; target: string; hasAttribute(name: string): boolean },
  origin: string,
): boolean {
  if (ev.defaultPrevented || ev.button !== 0 || ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey) return false;
  if (anchor.target && anchor.target !== '_self') return false;
  if (anchor.hasAttribute('download') || anchor.hasAttribute('data-native')) return false;
  let url: URL;
  try {
    url = new URL(anchor.href);
  } catch {
    return false;
  }
  if (url.origin !== origin) return false;
  // Daemon-served paths must hit the server.
  return !/^\/(api|mcp)(\/|$)/.test(url.pathname);
}
