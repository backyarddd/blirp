import { describe, expect, it } from 'vitest';
import { conflictCount, formatBytes, pickHubRoot } from './files';
import type { FilesRoot, ProjectFiles, RootInfo } from './api/types.gen';

function root(id: string, hub: Partial<RootInfo> | null): FilesRoot {
  return {
    root_id: id,
    machine_id: 'm',
    machine_name: 'laptop',
    path: `/p/${id}`,
    origin_revoked: false,
    hub: hub
      ? {
          root_id: id,
          project_id: 'p',
          machine_id: 'm',
          machine_name: 'laptop',
          origin_revoked: false,
          path: `/p/${id}`,
          head: 1,
          files: 1,
          bytes: 1,
          conflicts: 0,
          created_at: 0,
          updated_at: 0,
          manifest: null,
          incarnation: 'i',
          ...hub,
        }
      : null,
    local: null,
  };
}

function files(roots: FilesRoot[]): ProjectFiles {
  return { available: true, mode: 'default', global: true, effective: true, paused: false, hub_error: null, roots };
}

describe('files helpers', () => {
  it('formats sizes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(50 * 1024 * 1024)).toBe('50 MB');
    expect(formatBytes(2.5 * 1024 ** 3)).toBe('2.5 GB');
    expect(formatBytes(Number.NaN)).toBe('0 B');
  });

  it('picks the most recently updated root that has files on the hub', () => {
    expect(pickHubRoot(null)).toBeNull();
    const r = pickHubRoot(files([root('a', { updated_at: 5 }), root('b', { updated_at: 9 }), root('c', null), root('d', { updated_at: 20, files: 0 })]));
    expect(r?.root_id).toBe('b');
  });

  it('counts conflict copies over roots', () => {
    expect(conflictCount(files([root('a', { conflicts: 2 }), root('b', { conflicts: 1 }), root('c', null)]))).toBe(3);
    expect(conflictCount(undefined)).toBe(0);
  });
});
