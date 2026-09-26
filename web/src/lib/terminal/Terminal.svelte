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
  import {
    backoffDelay,
    classifyClose,
    decodeServerFrame,
    encodeBinaryInput,
    encodeInput,
    encodeResize,
  } from './protocol';

  type ConnState = 'connecting' | 'open' | 'reconnecting' | 'ended' | 'forbidden' | 'not_found';

  interface Props {
    sessionId: string;
    /** WebGL contexts are scarce (~16 per page); grid tiles may opt out. */
    webgl?: boolean;
    autofocus?: boolean;
    fontSize?: number;
    label: string;
  }

  let { sessionId, webgl = true, autofocus = false, fontSize = 13, label }: Props = $props();

  let host: HTMLDivElement | undefined = $state();
  let term: Terminal | undefined = $state.raw();
  let conn: ConnState = $state('connecting');

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

  // Only the host element and session id recreate the terminal; options update in place.
  $effect(() => {
    const el = host;
    const id = sessionId;
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

    const send = (data: Uint8Array<ArrayBuffer> | string): void => {
      if (ws?.readyState === WebSocket.OPEN) ws.send(data);
    };
    const sendResize = (): void => {
      if (t.cols > 0 && t.rows > 0) send(encodeResize(t.cols, t.rows));
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
        if (frame.type === 'snapshot') {
          // Every (re)attach starts from a full screen snapshot.
          t.reset();
          t.write(frame.data);
        } else if (frame.type === 'output') {
          t.write(frame.data);
        } else if (!warned) {
          warned = true;
          console.warn(`blirp: ignoring terminal frame (${frame.reason})`);
        }
      };
      sock.onclose = (ev) => {
        if (disposed || ws !== sock) return;
        ws = null;
        const kind = classifyClose(ev.code);
        if (kind === 'retry') {
          conn = 'reconnecting';
          timer = setTimeout(connect, backoffDelay(attempt++));
        } else {
          conn = kind;
        }
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

    term = t;
    refit();
    connect();
    if (autofocus) t.focus();

    return () => {
      disposed = true;
      clearTimeout(timer);
      cancelAnimationFrame(raf);
      ro.disconnect();
      ws?.close(1000);
      ws = null;
      term = undefined;
      t.dispose();
    };
  }

  const MESSAGES: Record<Exclude<ConnState, 'open'>, string> = {
    connecting: 'Connecting to terminal…',
    reconnecting: 'Connection lost. Reconnecting…',
    ended: 'The terminal has ended.',
    forbidden: 'This device is not allowed to control terminals. Enable terminal control for it in Settings > Machines & Sync.',
    not_found: 'This terminal is no longer running.',
  };
</script>

<div class="wrap" role="group" aria-label={label}>
  <div class="term" bind:this={host}></div>
  {#if conn !== 'open'}
    <div class="banner" class:warn={conn !== 'connecting'} role="status" aria-live="polite">{MESSAGES[conn]}</div>
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
</style>
