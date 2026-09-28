// What a paste or drop onto a terminal does. Browsers hand pastes to xterm as text only, so a
// pasted image or file would be lost: those are uploaded to the machine running the session
// instead (POST /api/sessions/:id/uploads) and their saved paths typed into the terminal, the way
// a native terminal types a dropped file's path. Agents attach image paths pasted like that.

/** Mirrors the daemon's limit (`uploads::MAX_BYTES`), checked here to fail before uploading. */
export const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

/** The parts of `DataTransfer` the decision reads (a real one in the app, a stub in tests). */
export interface TransferLike {
  readonly files: ArrayLike<File>;
  readonly items?: ArrayLike<{
    readonly kind: string;
    getAsFile(): File | null;
    webkitGetAsEntry?(): { readonly isDirectory: boolean } | null;
  }>;
  getData(format: string): string;
}

/** `folders`: dropped folders left out (only files are uploaded). */
export type TransferAction = { kind: 'text' } | { kind: 'upload'; files: File[]; folders: number };

function filesOf(dt: TransferLike): File[] {
  const files = Array.from(dt.files);
  if (files.length > 0 || !dt.items) return files;
  // Some browsers list a pasted image only as an item.
  return Array.from(dt.items)
    .filter((i) => i.kind === 'file')
    .map((i) => i.getAsFile())
    .filter((f): f is File => f !== null);
}

/**
 * Whether the clipboard's text only describes its files: empty, a single URL (copying an image
 * in a browser may also put its address there), or the files' names or paths
 * (copying files in a file manager).
 */
function textOnlyNamesFiles(text: string, files: File[]): boolean {
  if (text === '') return true;
  if (/^(https?|file|data|blob):\S*$/i.test(text)) return true;
  const lines = text.split(/\r?\n/).map((l) => l.trim().replace(/^["']|["']$/g, ''));
  const names = new Set(files.map((f) => f.name));
  return lines.every((l) => l === '' || names.has(l.split(/[\\/]/).pop() ?? l));
}

/**
 * A paste: plain text stays xterm's (bracketed paste and all). Files win when the text only
 * names them; when the clipboard carries real text next to an image (office apps add a picture
 * of copied cells or paragraphs), the text is what the user meant and is pasted as before.
 */
export function pasteAction(dt: TransferLike | null): TransferAction {
  if (!dt) return { kind: 'text' };
  const files = filesOf(dt);
  if (files.length === 0) return { kind: 'text' };
  return textOnlyNamesFiles(dt.getData('text/plain').trim(), files)
    ? { kind: 'upload', files, folders: 0 }
    : { kind: 'text' };
}

/**
 * A drop: files are uploaded, folders counted and skipped (browsers list a dropped folder as an
 * empty `File`); anything else (dragged text) is left to the browser. Must run inside the drop
 * event: its items are empty afterwards.
 */
export function dropAction(dt: TransferLike | null): TransferAction {
  if (!dt) return { kind: 'text' };
  if (!dt.items) {
    const files = Array.from(dt.files);
    return files.length > 0 ? { kind: 'upload', files, folders: 0 } : { kind: 'text' };
  }
  const files: File[] = [];
  let folders = 0;
  for (const item of Array.from(dt.items)) {
    if (item.kind !== 'file') continue;
    if (item.webkitGetAsEntry?.()?.isDirectory) {
      folders++;
      continue;
    }
    const f = item.getAsFile();
    if (f) files.push(f);
  }
  return files.length + folders > 0 ? { kind: 'upload', files, folders } : { kind: 'text' };
}


/** Longest text a program may put on the clipboard with OSC 52. */
export const OSC52_MAX_CHARS = 1024 * 1024;

/**
 * Whether a program's OSC 52 clipboard write goes through: only to the clipboard selection (`c`, or
 * the default), only while the user is typing into this pane (its terminal has focus) and may
 * control it, and only non-empty text under 1 MB, so a background session cannot fill or clear the
 * clipboard.
 */
export function osc52WriteAllowed(selection: string, text: string, focused: boolean, viewOnly: boolean): boolean {
  return (
    (selection === '' || selection.includes('c')) &&
    focused &&
    !viewOnly &&
    text.length > 0 &&
    text.length < OSC52_MAX_CHARS
  );
}
