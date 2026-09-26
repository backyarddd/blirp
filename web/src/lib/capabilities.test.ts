import { describe, expect, it } from 'vitest';
import { NONE_DENIED, denyFor, rightsFrom } from './capabilities';

const LOCAL = { admin: true, control_terminals: true, local: true };
const VIEWER = { admin: false, control_terminals: false, local: false };
const CONTROLLER = { admin: false, control_terminals: true, local: false };

describe('rightsFrom', () => {
  it('grants a local client everything', () => {
    expect(rightsFrom(LOCAL, NONE_DENIED)).toEqual({ admin: true, control: true, local: true });
  });

  it('keeps a view-only portal device read-only', () => {
    expect(rightsFrom(VIEWER, NONE_DENIED)).toEqual({ admin: false, control: false, local: false });
  });

  it('lets a controlling device use terminals but not admin actions', () => {
    expect(rightsFrom(CONTROLLER, NONE_DENIED)).toEqual({ admin: false, control: true, local: false });
  });

  it('grants nothing before capabilities are known', () => {
    expect(rightsFrom(null, NONE_DENIED)).toEqual({ admin: false, control: false, local: false });
    expect(rightsFrom(undefined, NONE_DENIED)).toEqual({ admin: false, control: false, local: false });
  });

  it('narrows stale capabilities by the 403s seen since', () => {
    const noControl = denyFor('control_not_allowed', NONE_DENIED);
    expect(rightsFrom(LOCAL, noControl)).toEqual({ admin: true, control: false, local: true });
    const noAdmin = denyFor('admin_only', NONE_DENIED);
    expect(rightsFrom(LOCAL, noAdmin)).toEqual({ admin: false, control: true, local: false });
  });
});

describe('denyFor', () => {
  it('ignores other error codes', () => {
    expect(denyFor('not_found', NONE_DENIED)).toBe(NONE_DENIED);
  });

  it('accumulates', () => {
    expect(denyFor('admin_only', denyFor('control_not_allowed', NONE_DENIED))).toEqual({ admin: true, control: true });
  });
});
