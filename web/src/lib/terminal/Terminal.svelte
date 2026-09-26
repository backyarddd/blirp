<script lang="ts">
  import { untrack } from 'svelte';
  import { Terminal, type ITheme } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import { WebglAddon } from '@xterm/addon-webgl';
  import { WebLinksAddon } from '@xterm/addon-web-links';
  import { Unicode11Addon } from '@xterm/addon-unicode11';
  import '@xterm/xterm/css/xterm.css';
  import { terminalWsPath, wsUrl } from '../api/client';
  import { theme } from '../theme.svelte';
  import { isMac } from '../prefs';
  import { matchShortcut } from '../shortcuts';
  import { app } from '../app.svelte';
  import { hasTerminal, sessionStatusInfo } from '../status';
  import type { SessionStatus } from '../api/types.gen';
  import { backoffDelay, decodeServerFrame, encodeBinaryInput, encodeInput, encodeResize } from './protocol';

  type ConnState = 'connecting' | 'open' | 'reconnecting' | 'exited' | 'ended';

  interface Props {
    sessionId: string;
    /** WebGL contexts are scarce (~16 per page); grid tiles may opt out. */
    webgl?: boolean;
    autofocus?: boolean;
    fontSize?: number;
    label: string;
    /** Offered in the exit banner, e.g. to switch to the transcript view. */
    ondetails?: () => void;
  }

  let { sessionId, webgl = true, autofocus = false, fontSize = 13, label, ondetails }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let term: Terminal | undefined = $state.raw();
  let conn: ConnState = $state('connecting');
  let exit = $state<{ status: SessionStatus; exit_code: number | null } | null>(null);

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

  // Devices without terminal control get a view-only stream: the daemon drops their input and
  // resize frames without telling the client, so say so here instead of swallowing keys.
  const viewOnly = $derived(!app.control);
  $effect(() => {
    if (term && exit === null) term.options.disableStdin = viewOnly;
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

  function mount(el: HTMLDivElement, id: string): () => void {
    const t = new Terminal({
      allowProposedApi: true, // required by the unicode11 addon
      cursorBlink: true,
      fontFamily: getComputedStyle(document.documentElement).getPropertyValue('--mono').trim() || 'monospace',
      fontSize,
      scrollback: 10_000,
      theme: theme.resolved === 'dark' ? DARK : LIGHT,
      macOptionIsMeta: true,
    });
    const fit = new FitAddon();
    t.loadAddon(fit);
    t.loadAddon(new WebLinksAddon((_ev, uri) => window.open(uri, '_blank', 'noopener,noreferrer')));
    t.loadAddon(new Unicode11Addon());
    t.unicode.activeVersion = '11';
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
      if (cols === t.cols && rows === t.rows) return;
      remoteResize = true;
      try {
        t.resize(cols, rows);
      } finally {
        remoteResize = false;
      }
    };
    const refit = (): void => {
      if (el.clientWidth === 0 || el.clientHeight === 0) return; // hidden
      try {
        fit.fit();
      } catch (e) {
        console.warn('blirp: terminal fit failed', e);
      }
    };

    const connect = (): void => {
      conn = attempt === 0 ? 'connecting' : 'reconnecting';
      const sock = new WebSocket(wsUrl(terminalWsPath(id)));
      sock.binaryType = 'arraybuffer';
      ws = sock;
      sock.onopen = () => {
        attempt = 0;
        conn = 'open';
        refit();
        sendResize();
      };
      sock.onmessage = (ev: MessageEvent<unknown>) => {
        const data = ev.data;
        if (typeof data !== 'string' && !(data instanceof ArrayBuffer)) return;
        const frame = decodeServerFrame(data);
        switch (frame.type) {
          case 'snapshot':
            // Sent on attach and whenever this client fell behind. The snapshot is laid out for
            // the PTY's size at that moment; our own resize (sent on open) follows as `resize`.
            applyRemoteSize(frame.cols, frame.rows);
            t.reset();
            t.write(frame.data);
            break;
          case 'output':
            t.write(frame.data);
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
    };

    t.onData((d) => send(encodeInput(d)));
    t.onBinary((d) => send(encodeBinaryInput(d)));
    t.onResize(() => sendResize());

    t.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true;
      const key = e.key.toLowerCase();
      const clip = isMac ? e.metaKey && !e.ctrlKey && !e.altKey : e.ctrlKey && e.shiftKey && !e.altKey;
      if (clip && key === 'c') {
        e.preventDefault();
        const sel = t.getSelection();
        if (sel) {
          navigator.clipboard.writeText(sel).catch((err: unknown) => {
            app.toast(`Copy failed: ${err instanceof Error ? err.message : String(err)}`);
          });
        }
        return false;
      }
      // Let the browser raise a native paste event, which xterm turns into input
      // (bracketed paste aware) without needing clipboard-read permission.
      if (clip && key === 'v') return false;
      // App shortcuts bubble to the global handler instead of reaching the PTY.
      if (matchShortcut(e, isMac, true)) return false;
      return true;
    });

    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(refit);
    });
    ro.observe(el);
    // Another client may have resized the PTY; typing here takes the size back.
    const reclaim = (): void => refit();
    t.textarea?.addEventListener('focus', reclaim);

    term = t;
    refit();
    connect();
    if (autofocus) t.focus();

    return () => {
      disposed = true;
      clearTimeout(timer);
      cancelAnimationFrame(raf);
      ro.disconnect();
      t.textarea?.removeEventListener('focus', reclaim);
      ws?.close(1000);
      ws = null;
      term = undefined;
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
      ? `Process exited · ${sessionStatusInfo({ ...app.sessionById.get(sid), status: exit.status }).label}${exit.exit_code !== null ? ` (exit code ${exit.exit_code})` : ''}`
      : '',
  );
</script>

<div class="wrap" role="group" aria-label={label}>
  <div class="term" bind:this={host}></div>
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
      View only: this device may not type into terminals. Allow it under Settings &gt; Machines &amp; Sync on the hub.
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
