import { describe, expect, it } from 'vitest';
import { OSC52_MAX_CHARS, dropAction, osc52WriteAllowed, pasteAction, type TransferLike } from './paste';

const png = (name = 'image.png'): File => new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], name, { type: 'image/png' });

function transfer(files: File[], text = '', asItems = false): TransferLike {
  return {
    files: asItems ? [] : files,
    items: files.map((f) => ({ kind: 'file', getAsFile: () => f })),
    getData: (format) => (format === 'text/plain' ? text : ''),
  };
}

describe('pasteAction', () => {
  it('leaves plain text to xterm', () => {
    expect(pasteAction(transfer([], 'echo hi'))).toEqual({ kind: 'text' });
    expect(pasteAction(null)).toEqual({ kind: 'text' });
  });

  it('uploads a pasted screenshot', () => {
    const f = png();
    expect(pasteAction(transfer([f]))).toEqual({ kind: 'upload', files: [f], folders: 0 });
  });

  it('finds images listed only as clipboard items', () => {
    const f = png();
    expect(pasteAction(transfer([f], '', true))).toEqual({ kind: 'upload', files: [f], folders: 0 });
  });

  it('prefers the image when the text is only its address or file name', () => {
    const f = png('photo.jpg');
    for (const text of [
      'https://example.com/photo.jpg',
      'data:image/png;base64,AAAA',
      'photo.jpg',
      '/home/user/Pictures/photo.jpg',
      '"C:\\Users\\user\\Pictures\\photo.jpg"',
    ]) {
      expect(pasteAction(transfer([f], text)), text).toEqual({ kind: 'upload', files: [f], folders: 0 });
    }
  });

  it('uploads every copied file', () => {
    const a = png('a.png');
    const b = new File(['x'], 'notes.txt');
    expect(pasteAction(transfer([a, b], 'a.png\nnotes.txt'))).toEqual({ kind: 'upload', files: [a, b], folders: 0 });
  });

  it('keeps real text that comes with a rendered picture of it', () => {
    expect(pasteAction(transfer([png()], 'Quarterly total\t42'))).toEqual({ kind: 'text' });
    expect(pasteAction(transfer([png()], 'see https://example.com'))).toEqual({ kind: 'text' });
  });
});

describe('dropAction', () => {
  it('uploads dropped files and leaves dragged text alone', () => {
    const f = png();
    expect(dropAction(transfer([f]))).toEqual({ kind: 'upload', files: [f], folders: 0 });
    expect(dropAction(transfer([], 'some text'))).toEqual({ kind: 'text' });
    expect(dropAction(null)).toEqual({ kind: 'text' });
  });

  it('skips dropped folders and counts them', () => {
    const f = png();
    const folder = new File([], 'photos');
    const dt: TransferLike = {
      files: [f, folder],
      items: [
        { kind: 'file', getAsFile: () => f, webkitGetAsEntry: () => ({ isDirectory: false }) },
        { kind: 'file', getAsFile: () => folder, webkitGetAsEntry: () => ({ isDirectory: true }) },
      ],
      getData: () => '',
    };
    expect(dropAction(dt)).toEqual({ kind: 'upload', files: [f], folders: 1 });
    const onlyFolder: TransferLike = { ...dt, items: [dt.items?.[1] ?? { kind: 'string', getAsFile: () => null }] };
    expect(dropAction(onlyFolder)).toEqual({ kind: 'upload', files: [], folders: 1 });
  });
});

describe('osc52WriteAllowed', () => {
  it('lets a focused, controllable pane set the clipboard', () => {
    expect(osc52WriteAllowed('c', 'hello', true, false)).toBe(true);
    expect(osc52WriteAllowed('', 'hello', true, false)).toBe(true);
  });
  it('refuses unfocused or view-only panes, other selections, empty and huge text', () => {
    expect(osc52WriteAllowed('c', 'hello', false, false)).toBe(false);
    expect(osc52WriteAllowed('c', 'hello', true, true)).toBe(false);
    expect(osc52WriteAllowed('p', 'hello', true, false)).toBe(false);
    expect(osc52WriteAllowed('c', '', true, false)).toBe(false);
    expect(osc52WriteAllowed('c', 'x'.repeat(OSC52_MAX_CHARS), true, false)).toBe(false);
    expect(osc52WriteAllowed('c', 'x'.repeat(OSC52_MAX_CHARS - 1), true, false)).toBe(true);
  });
});
