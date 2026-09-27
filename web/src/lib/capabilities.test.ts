import { describe, expect, it } from 'vitest';
import { NONE_DENIED, denyFor, remoteRefusal, rightsFrom } from './capabilities';

const LOCAL = { admin: true, control_terminals: true, local: true, files: true };
const VIEWER = { admin: false, control_terminals: false, local: false, files: false };
const CONTROLLER = { admin: false, control_terminals: true, local: false, files: true };

describe('rightsFrom', () => {
  it('grants a local client everything', () => {
    expect(rightsFrom(LOCAL, NONE_DENIED)).toEqual({ admin: true, control: true, local: true, files: true });
  });

  it('keeps a view-only portal device read-only', () => {
    expect(rightsFrom(VIEWER, NONE_DENIED)).toEqual({ admin: false, control: false, local: false, files: false });
  });

  it('lets a controlling device use terminals but not admin actions', () => {
    expect(rightsFrom(CONTROLLER, NONE_DENIED)).toEqual({ admin: false, control: true, local: false, files: true });
  });

  it('grants nothing before capabilities are known', () => {
    expect(rightsFrom(null, NONE_DENIED)).toEqual({ admin: false, control: false, local: false, files: false });
    expect(rightsFrom(undefined, NONE_DENIED)).toEqual({ admin: false, control: false, local: false, files: false });
  });

  it('narrows stale capabilities by the 403s seen since', () => {
    const noControl = denyFor('control_not_allowed', NONE_DENIED);
    expect(rightsFrom(LOCAL, noControl)).toEqual({ admin: true, control: false, local: true, files: true });
    const noAdmin = denyFor('admin_only', NONE_DENIED);
    expect(rightsFrom(LOCAL, noAdmin)).toEqual({ admin: false, control: true, local: false, files: true });
    const noFiles = denyFor('files_not_allowed', NONE_DENIED);
    expect(rightsFrom(LOCAL, noFiles)).toEqual({ admin: true, control: true, local: true, files: false });
  });
});

describe('denyFor', () => {
  it('ignores other error codes', () => {
    expect(denyFor('not_found', NONE_DENIED)).toBe(NONE_DENIED);
  });

  it('accumulates', () => {
    expect(denyFor('admin_only', denyFor('control_not_allowed', NONE_DENIED))).toEqual({ admin: true, control: true, files: false });
  });
});

describe('remoteRefusal', () => {
  it('explains why the owning machine refused', () => {
    expect(remoteRefusal('machine_unreachable', 'laptop')).toMatch(/^laptop is offline/);
    expect(remoteRefusal('control_not_allowed', 'laptop')).toContain('Allow the hub to control this machine');
    expect(remoteRefusal('proxy_failed', 'laptop')).toMatch(/did not answer/);
    expect(remoteRefusal('session_live', 'laptop')).toBeNull();
  });
});
