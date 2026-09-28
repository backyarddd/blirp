<script lang="ts">
  import { untrack } from 'svelte';
  import { Terminal, type ITheme } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import { WebglAddon } from '@xterm/addon-webgl';
  import { WebLinksAddon } from '@xterm/addon-web-links';
  import { Unicode11Addon } from '@xterm/addon-unicode11';
  import { SearchAddon } from '@xterm/addon-search';
  import { ClipboardAddon } from '@xterm/addon-clipboard';
  import '@xterm/xterm/css/xterm.css';
  import { api, errorMessage, socketUrl, terminalWsPath } from '../api/client';
  import { theme } from '../theme.svelte';
  import { isMac, isWindows } from '../prefs';
  import { matchShortcut, terminalClipboardKey } from '../shortcuts';
  import { app } from '../app.svelte';
  import { hasTerminal, sessionStatusInfo } from '../status';
  import type { SessionStatus } from '../api/types.gen';
  import { backoffDelay, decodeServerFrame, encodeBinaryInput, encodeInput, encodeResize } from './protocol';
  import { MAX_UPLOAD_BYTES, dropAction, pasteAction } from './paste';
  import { LIMITS, terminalSettings } from './settings.svelte';
  import TerminalFind from './TerminalFind.svelte';

  // The terminal follows VS Code's integrated terminal (xtermTerminal.ts, terminalInstance.ts):
  // the same xterm.js options and addons, every key goes to the program except the app shortcuts in
  // shortcuts.ts, copy/paste/find keys and right click as VS Code binds them, and the daemon's
  // snapshot restores screen and modes on every attach (pty/modes.rs).

  type ConnState = 'connecting' | 'open' | 'reconnecting' | 'exited' | 'ended';

  interface Props {
    sessionId: string;
    /** WebGL contexts are scarce (~16 per page); grid tiles may opt out. */
    webgl?: boolean;
    autofocus?: boolean;
    /** One point smaller than the configured font (grid tiles). */
    compact?: boolean;
    label: string;
    /** Offered in the exit banner, e.g. to switch to the transcript view. */
    ondetails?: () => void;
  }

  let { sessionId, webgl = true, autofocus = false, compact = false, label, ondetails }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let term: Terminal | undefined = $state.raw();
  let search: SearchAddon | undefined = $state.raw();
  let conn: ConnState = $state('connecting');
  let exit = $state<{ status: SessionStatus; exit_code: number | null } | null>(null);
  /** The open find widget and the text it starts with. */
  let find = $state<{ initial: string } | null>(null);

  const LIGHT: ITheme = {
    background: '#ffffff',
    foreground: '#1f2328',
    cursor: '#f2542d',
    cursorAccent: '#ffffff',
    selectionBackground: 'rgba(242, 84, 45, 0.22)',
    black: '#24292f',
    red: '#cf222e',
    green: '#116329',
    yellow: '#9a6700',
    blue: '#0969da',
    magenta: '#8250df',
    cyan: '#1b7c83',
    white: '#6e7781',
    brightBlack: '#57606a',
    brightRed: '#a40e26',
    brightGreen: '#1a7f37',
    brightYellow: '#7d4e00',
    brightBlue: '#218bff',
    brightMagenta: '#a475f9',
    brightCyan: '#3192aa',
    brightWhite: '#8c959f',
  };
  const DARK: ITheme = {
    background: '#15161a',
    foreground: '#e6e6e6',
    cursor: '#f2643f',
    cursorAccent: '#15161a',
    selectionBackground: 'rgba(242, 100, 63, 0.3)',
    black: '#1e1f24',
    red: '#ff6b61',
    green: '#4cc27f',
    yellow: '#e8b24a',
    blue: '#6c9bff',
    magenta: '#c49bff',
    cyan: '#4fc4cf',
    white: '#d0d3d9',
    brightBlack: '#6b7080',
    brightRed: '#ff8a80',
    brightGreen: '#6fdc9c',
    brightYellow: '#ffd173',
    brightBlue: '#94b6ff',
    brightMagenta: '#dcbcff',
    brightCyan: '#7fe0e8',
    brightWhite: '#ffffff',
  };

  $effect(() => {
    if (term) term.options.theme = theme.resolved === 'dark' ? DARK : LIGHT;
  });

  // Clients without terminal control get a view-only stream: the daemon says so with a
  // `readonly` frame and drops their input and resize frames, so say so here instead of
  // swallowing keys. Capabilities cover it before the frame arrives.
  let readonlyFrame = $state(false);
  const viewOnly = $derived(!app.control || readonlyFrame);
  $effect(() => {
    if (term && exit === null) term.options.disableStdin = viewOnly;
  });

  // Settings > Appearance > Terminal, applied in place; the cell size changes, so fit again.
  const fontSize = $derived(Math.max(LIMITS.fontSize.min, terminalSettings.fontSize - (compact ? 1 : 0)));
  let fitAfterFontChange: (() => void) | undefined;
  $effect(() => {
    const t = term;
    if (!t) return;
    t.options.fontSize = fontSize;
    t.options.lineHeight = terminalSettings.lineHeight;
    t.options.letterSpacing = terminalSettings.letterSpacing;
    t.options.cursorStyle = terminalSettings.cursorStyle;
    t.options.macOptionIsMeta = terminalSettings.macOptionIsMeta;
    if (exit === null) t.options.cursorBlink = terminalSettings.cursorBlink;
    untrack(() => fitAfterFontChange?.());
  });

  // Only the host element and session id recreate the terminal; options update in place.
  // `sessionId` is usually passed as `session.id`, which re-fires on every status push; the
  // derived only changes with the value, so a status update never drops the connection.
  const sid = $derived(sessionId);
  $effect(() => {
    const el = host;
    const id = sid;
    if (!el) return;
    return untrack(() => mount(el, id));
  });

  function copyText(text: string): void {
    navigator.clipboard.writeText(text).catch((err: unknown) => {
      app.toast(`Copy failed: ${err instanceof Error ? err.message : String(err)}`);
    });
  }

  function mount(el: HTMLDivElement, id: string): () => void {
    const s = terminalSettings;
    const t = new Terminal({
      allowProposedApi: true, // unicode11, search decorations
      fontFamily: getComputedStyle(document.documentElement).getPropertyValue('--mono').trim() || 'monospace',
      fontSize,
      lineHeight: s.lineHeight,
      letterSpacing: s.letterSpacing,
      cursorStyle: s.cursorStyle,
      cursorBlink: s.cursorBlink,
      cursorInactiveStyle: 'outline',
      // The daemon keeps as many lines for snapshots (pty.rs SCROLLBACK).
      scrollback: 10_000,
      theme: theme.resolved === 'dark' ? DARK : LIGHT,
      macOptionIsMeta: s.macOptionIsMeta,
      macOptionClickForcesSelection: false,
      rightClickSelectsWord: isMac,
      minimumContrastRatio: 4.5,
      drawBoldTextInBrightColors: true,
      rescaleOverlappingGlyphs: true,
      scrollOnEraseInDisplay: true,
      wordSeparator: ' ()[]{}\',"`─‘’“”|',
      windowOptions: { getWinSizePixels: true, getCellSizePixels: true, getWinSizeChars: true },
      // Kitty keyboard reporting when a program asks for it (Shift+Enter, Ctrl+letter chords);
      // win32-input-mode stays off, as in VS Code: it would win over kitty and ConPTY does not
      // pass kitty sequences on to programs from win32 input records.
      vtExtensions: { kittyKeyboard: true, win32InputMode: false },
      // OSC 8 hyperlinks: web links only, opened like the ones the links addon finds.
      linkHandler: {
        activate: (_ev, uri) => window.open(uri, '_blank', 'noopener,noreferrer'),
        allowNonHttpProtocols: false,
      },
    });
    const fit = new FitAddon();
    t.loadAddon(fit);
    t.loadAddon(new WebLinksAddon((_ev, uri) => window.open(uri, '_blank', 'noopener,noreferrer')));
    t.loadAddon(new Unicode11Addon());
    t.unicode.activeVersion = '11';
    const finder = new SearchAddon({ highlightLimit: 1000 });
    t.loadAddon(finder);
    // OSC 52: programs (Claude Code, tmux, vim) may set the clipboard, as in VS Code. Reading it
    // back is refused: a session can run on another machine and must not see this clipboard.
    t.loadAddon(
      new ClipboardAddon(undefined, {
        readText: () => '',
        writeText: (selection, text) => {
          if (selection !== '' && !selection.includes('c')) return;
          navigator.clipboard.writeText(text).catch((err: unknown) => {
            console.warn('blirp: a program could not set the clipboard (OSC 52)', err);
          });
        },
      }),
    );
    t.open(el);
    if (webgl) {
      try {
        const gl = new WebglAddon();
        // Disposing on context loss makes xterm fall back to its DOM renderer.
        gl.onContextLoss(() => gl.dispose());
        t.loadAddon(gl);
      } catch (e) {
        console.warn('blirp: WebGL renderer unavailable, using the DOM renderer', e);
      }
    }

    let ws: WebSocket | null = null;
    let attempt = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let colsTimer: ReturnType<typeof setTimeout> | undefined;
    let disposed = false;
    let warned = false;
    // Set while applying a size the daemon reported, so it is not echoed back as our own.
    let remoteResize = false;

    const send = (data: Uint8Array<ArrayBuffer> | string): void => {
      if (ws?.readyState === WebSocket.OPEN) ws.send(data);
    };
    const sendResize = (): void => {
      if (!remoteResize && !viewOnly && t.cols > 0 && t.rows > 0) send(encodeResize(t.cols, t.rows));
    };
    const applyRemoteSize = (cols: number, rows: number): void => {
      clearTimeout(colsTimer);
      if (cols === t.cols && rows === t.rows) return;
      remoteResize = true;
      try {
        t.resize(cols, rows);
      } finally {
        remoteResize = false;
      }
    };
    // VS Code's TerminalResizeDebouncer: with a long buffer, rows follow at once but columns wait
    // 100 ms, since a column change reflows every line.
    const refit = (immediate = false): void => {
      if (el.clientWidth === 0 || el.clientHeight === 0) return; // hidden
      const d = fit.proposeDimensions();
      if (!d || !Number.isFinite(d.cols) || !Number.isFinite(d.rows)) return;
      if (immediate || t.buffer.normal.length < 200) {
        clearTimeout(colsTimer);
        if (d.cols !== t.cols || d.rows !== t.rows) t.resize(d.cols, d.rows);
        return;
      }
      if (d.rows !== t.rows) t.resize(t.cols, d.rows);
      clearTimeout(colsTimer);
      if (d.cols !== t.cols) colsTimer = setTimeout(() => t.resize(d.cols, t.rows), 100);
    };
    fitAfterFontChange = () => refit(true);

    const retry = (): void => {
      // The daemon uses no close codes (see protocol.ts). A refused upgrade or a drop looks the
      // same, so keep retrying only while the daemon still reports the session as live.
      const s = app.sessionById.get(id);
      if (s && !hasTerminal(s)) {
        conn = 'ended';
        return;
      }
      conn = 'reconnecting';
      timer = setTimeout(connect, backoffDelay(attempt++));
    };

    const connect = (): void => {
      conn = attempt === 0 ? 'connecting' : 'reconnecting';
      readonlyFrame = false; // access may have changed (the daemon closes with 1001 then)
      socketUrl(terminalWsPath(id)).then(
        (url) => {
          if (!disposed) open(url);
        },
        () => {
          if (!disposed) retry();
        },
      );
    };

    const open = (url: string): void => {
      const sock = new WebSocket(url);
      sock.binaryType = 'arraybuffer';
      ws = sock;
      sock.onopen = () => {
        attempt = 0;
        conn = 'open';
        refit(true);
        sendResize();
      };
      sock.onmessage = (ev: MessageEvent<unknown>) => {
        const data = ev.data;
        if (typeof data !== 'string' && !(data instanceof ArrayBuffer)) return;
        const frame = decodeServerFrame(data);
        switch (frame.type) {
          case 'snapshot':
            // Sent on attach and whenever this client fell behind, laid out for the PTY's size at
            // that moment. Afterwards the pane takes the PTY back to its own size: the resize sent
            // on open may have reached the daemon before the snapshot was taken, and the daemon
            // does not echo a client's own resize.
            if (frame.windows_pty) {
              t.options.windowsPty = { backend: 'conpty', buildNumber: frame.windows_pty.build_number };
              // As VS Code does with its bundled conpty.dll, which reflows the cursor line itself.
              t.options.reflowCursorLine = frame.windows_pty.bundled_conpty;
            }
            applyRemoteSize(frame.cols, frame.rows);
            t.reset();
            t.write(frame.data, () => {
              if (!disposed) refit(true);
            });
            break;
          case 'output':
            t.write(frame.data);
            break;
          case 'readonly':
            readonlyFrame = true;
            break;
          case 'resize':
            // Last resize wins (§6): mirror the PTY size so output wraps the way the app drew it.
            applyRemoteSize(frame.cols, frame.rows);
            break;
          case 'exit':
            exit = { status: frame.status, exit_code: frame.exit_code };
            conn = 'exited';
            t.options.disableStdin = true;
            t.options.cursorBlink = false;
            break;
          case 'ignored':
            if (!warned) {
              warned = true;
              console.warn(`blirp: ignoring terminal frame (${frame.reason})`);
            }
        }
      };
      sock.onclose = () => {
        if (disposed || ws !== sock) return;
        ws = null;
        if (conn === 'exited') return;
        retry();
      };
    };

    t.onData((d) => send(encodeInput(d)));
    t.onBinary((d) => send(encodeBinaryInput(d)));
    t.onResize(() => sendResize());

    const platform = isMac ? 'mac' : isWindows ? 'windows' : 'linux';
    t.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true;
      const clip = terminalClipboardKey(e, platform, t.hasSelection());
      if (clip === 'copy' || clip === 'copy-clear') {
        e.preventDefault();
        copyText(t.getSelection());
        if (clip === 'copy-clear') t.clearSelection();
        return false;
      }
      // Let the browser raise a native paste event, which xterm turns into input
      // (bracketed paste aware) without needing clipboard-read permission.
      // Pasted files and images are uploaded instead (onPaste below).
      if (clip === 'paste') return false;
      // Find: Ctrl+F (Cmd+F on macOS), as VS Code binds it in a focused terminal.
      if (e.key.toLowerCase() === 'f' && !e.altKey && !e.shiftKey && (isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey)) {
        e.preventDefault();
        find = { initial: t.hasSelection() && !t.getSelection().includes('\n') ? t.getSelection() : '' };
        return false;
      }
      // App shortcuts bubble to the global handler instead of reaching the PTY.
      if (matchShortcut(e, isMac, true)) return false;
      // Cmd chords belong to the app and the browser on macOS and never reach the PTY; with the
      // kitty keyboard protocol xterm.js would encode them (VS Code skips them the same way).
      if (isMac && e.metaKey) return false;
      // Alt+F4 closes the window on Windows.
      if (isWindows && e.altKey && !e.ctrlKey && e.key === 'F4') return false;
      // Shift+Tab goes to the program and must not move focus out of the pane.
      if (e.key === 'Tab' && e.shiftKey) e.preventDefault();
      return true;
    });

    // Right click on Windows copies the selection, or pastes without one (VS Code's default there);
    // Shift+right click opens the menu. macOS selects the word under the pointer (xterm option).
    const onContextMenu = (e: MouseEvent): void => {
      if (!isWindows || e.shiftKey) return;
      e.preventDefault();
      if (t.hasSelection()) {
        copyText(t.getSelection());
        t.clearSelection();
        return;
      }
      if (viewOnly) return;
      navigator.clipboard.readText().then(
        (text) => {
          if (!disposed && text) t.paste(text);
        },
        (err: unknown) => app.toast(`Paste failed: ${err instanceof Error ? err.message : String(err)}`),
      );
    };
    el.addEventListener('contextmenu', onContextMenu);
    const onTouch = (): void => t.focus();
    el.addEventListener('touchstart', onTouch, { passive: true });

    // Pasted images and files, and files dropped on the pane (paste.ts): saved on the machine that
    // runs the session, then their paths are pasted one by one, as a native terminal types a
    // dropped file, so an agent can attach each. Plain text pastes stay with xterm.
    const uploadFiles = async (files: File[], folders = 0): Promise<void> => {
      if (folders > 0) app.toast(folders === 1 ? "Folders aren't supported; drop the files inside it." : "Folders aren't supported; drop the files inside them.");
      if (files.length === 0) return;
      if (viewOnly) {
        app.toast('Read-only: this device may not control terminals, so files cannot be pasted here.');
        return;
      }
      const fits = files.filter((f) => {
        if (f.size <= MAX_UPLOAD_BYTES) return true;
        app.toast(`${f.name} is larger than 25 MB and was not uploaded.`);
        return false;
      });
      if (fits.length === 0) return;
      app.toast(fits.length === 1 ? `Uploading ${fits[0]?.name ?? 'file'}…` : `Uploading ${fits.length} files…`, 'info');
      // One at a time: the daemon holds each upload in memory, and the session's quota is checked
      // per file.
      const saved: string[] = [];
      for (const f of fits) {
        try {
          saved.push((await api.sessions.upload(id, f)).quoted);
        } catch (e) {
          app.toast(`Could not upload ${f.name}: ${errorMessage(e)}`);
        }
        if (disposed) return;
      }
      if (saved.length === 0) return;
      // Typing into a closed socket would drop the paths without a trace.
      if (ws?.readyState !== WebSocket.OPEN) {
        app.toast(`Saved ${saved.join(' ')}, but the terminal is not connected; paste the path yourself.`);
        return;
      }
      saved.forEach((q, i) => {
        if (i > 0) send(encodeInput(' '));
        t.paste(q);
      });
      send(encodeInput(' '));
      t.focus();
    };
    // Capture phase: runs before xterm's own paste listener, which would paste nothing for an image.
    const onPaste = (e: ClipboardEvent): void => {
      const action = pasteAction(e.clipboardData);
      if (action.kind !== 'upload') return;
      e.preventDefault();
      e.stopPropagation();
      void uploadFiles(action.files, action.folders);
    };
    const onDragOver = (e: DragEvent): void => {
      if (!e.dataTransfer?.types.includes('Files')) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = viewOnly ? 'none' : 'copy';
    };
    const onDrop = (e: DragEvent): void => {
      const action = dropAction(e.dataTransfer);
      if (action.kind !== 'upload') return;
      e.preventDefault();
      void uploadFiles(action.files, action.folders);
    };
    el.addEventListener('paste', onPaste, true);
    el.addEventListener('dragover', onDragOver);
    el.addEventListener('drop', onDrop);

    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => refit());
    });
    ro.observe(el);
    // Another client may have resized the PTY; typing here takes the size back.
    const reclaim = (): void => refit(true);
    t.textarea?.addEventListener('focus', reclaim);

    term = t;
    search = finder;
    refit(true);
    connect();
    if (autofocus) t.focus();

    return () => {
      disposed = true;
      clearTimeout(timer);
      clearTimeout(colsTimer);
      cancelAnimationFrame(raf);
      ro.disconnect();
      t.textarea?.removeEventListener('focus', reclaim);
      el.removeEventListener('contextmenu', onContextMenu);
      el.removeEventListener('touchstart', onTouch);
      el.removeEventListener('paste', onPaste, true);
      el.removeEventListener('dragover', onDragOver);
      el.removeEventListener('drop', onDrop);
      ws?.close(1000);
      ws = null;
      fitAfterFontChange = undefined;
      term = undefined;
      search = undefined;
      find = null;
      t.dispose();
    };
  }

  const MESSAGES: Record<Exclude<ConnState, 'open' | 'exited'>, string> = {
    connecting: 'Connecting to terminal…',
    reconnecting: 'Connection lost. Reconnecting…',
    ended: 'This terminal is no longer running.',
  };

  const exitText = $derived(
    exit
      ? `Process exited · ${sessionStatusInfo({ status: exit.status, stopped_by_user: app.sessionById.get(sid)?.stopped_by_user ?? false }).label}${exit.exit_code !== null ? ` (exit code ${exit.exit_code})` : ''}`
      : '',
  );
</script>

<div class="wrap" role="group" aria-label={label}>
  <div class="term" bind:this={host}></div>
  {#if find && search}
    <TerminalFind
      {search}
      initial={find.initial}
      mac={isMac}
      onclose={() => {
        find = null;
        term?.focus();
      }}
    />
  {/if}
  {#if conn === 'exited'}
    <div class="banner exit" class:failed={exit?.status === 'failed'} role="status" aria-live="polite" data-testid="terminal-exit">
      {exitText}
      {#if ondetails}<button type="button" class="btn sm" onclick={ondetails}>Show details</button>{/if}
    </div>
  {:else if conn !== 'open'}
    <div class="banner" class:warn={conn !== 'connecting'} role="status" aria-live="polite">
      {MESSAGES[conn]}
      {#if conn === 'ended' && ondetails}<button type="button" class="btn sm" onclick={ondetails}>Show details</button>{/if}
    </div>
  {:else if viewOnly}
    <div class="banner view-only" role="status" data-testid="terminal-view-only">
      Read-only: typing here is disabled because this device may not control terminals. Allow it under Settings &gt;
      Machines &amp; Sync on the hub.
    </div>
  {/if}
</div>

<style>
  .wrap {
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 0;
    background: var(--panel);
  }
  .term {
    position: absolute;
    inset: 8px 4px 4px 10px;
  }
  :global(:root[data-theme='dark']) .wrap {
    background: #15161a;
  }
  .banner {
    position: absolute;
    top: 10px;
    left: 50%;
    transform: translateX(-50%);
    max-width: calc(100% - 24px);
    padding: 6px 12px;
    border-radius: 999px;
    background: var(--panel);
    border: 1px solid var(--border);
    box-shadow: var(--shadow-lg);
    font-size: 12.5px;
    color: var(--text-2);
    z-index: 5;
  }
  .banner.warn {
    color: var(--waiting);
  }
  .banner.exit {
    display: flex;
    align-items: center;
    gap: 10px;
    top: auto;
    bottom: 12px;
    padding: 4px 6px 4px 14px;
    color: var(--text);
  }
  .banner.view-only {
    top: auto;
    bottom: 12px;
  }
  .banner.exit.failed {
    color: var(--failed);
  }
</style>
