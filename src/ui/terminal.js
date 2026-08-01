// The integrated terminal. Text frames are control messages and binary frames
// are bytes, so a resize can never be typed into the shell by accident.

import { socketUrl } from './api.js';

const THEMES = {
  dark: {
    background: '#011627',
    foreground: '#d6deeb',
    cursor: '#80a4c2',
    selectionBackground: '#1d3b53',
  },
  light: {
    background: '#fbfbfb',
    foreground: '#403f53',
    cursor: '#90a7b2',
    selectionBackground: '#e0e0e0',
  },
};

export function createTerminal(element, { onStatus }) {
  let term = null;
  let fit = null;
  let socket = null;
  let observer = null;
  let shell = null;
  let theme = 'dark';

  const encoder = new TextEncoder();

  function send(control) {
    if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(control));
  }

  function connect() {
    const query = shell ? `?shell=${encodeURIComponent(shell)}` : '';
    const opening = new WebSocket(socketUrl(`/ws/terminal${query}`));
    socket = opening;
    opening.binaryType = 'arraybuffer';

    opening.addEventListener('open', () => {
      fit.fit();
      send({ type: 'resize', cols: term.cols, rows: term.rows });
    });
    opening.addEventListener('message', ({ data }) => term.write(new Uint8Array(data)));
    opening.addEventListener('close', () => {
      // Only report the death of the socket we still consider current — a
      // restart closes the old one on purpose.
      if (socket === opening) {
        socket = null;
        term.writeln('\r\n\x1b[2m[shell exited — ↻ or reopen the pane to start another]\x1b[0m');
      }
    });
    opening.addEventListener('error', () => onStatus('terminal connection failed', true));
  }

  function build() {
    term = new Terminal({
      fontFamily: 'ui-monospace, "Cascadia Mono", Consolas, monospace',
      fontSize: 12,
      cursorBlink: true,
      scrollback: 5000,
      theme: THEMES[theme],
    });
    fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(element);

    // Registered once for the life of the terminal, not once per connection —
    // otherwise every restart would double each keystroke.
    term.onData((data) => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(encoder.encode(data));
    });
    term.onResize(({ cols, rows }) => send({ type: 'resize', cols, rows }));

    observer = new ResizeObserver(() => {
      // Fitting a hidden element computes nonsense dimensions.
      if (element.offsetParent !== null) fit.fit();
    });
    observer.observe(element);
  }

  return {
    /** Build on first open; afterwards just re-fit and reconnect if needed. */
    open() {
      if (!term) build();
      if (!socket) connect();
      fit.fit();
      term.focus();
    },

    /** Switch shell and start a fresh session with it. */
    setShell(name) {
      shell = name;
      this.restart();
    },

    /**
     * Start a fresh session. Needed after the workspace changes: the working
     * directory is decided when the shell is spawned.
     */
    restart() {
      if (!term) return;
      socket?.close();
      socket = null;
      term.reset();
      connect();
      term.focus();
    },

    setTheme(resolved) {
      theme = resolved;
      if (term) term.options.theme = THEMES[resolved];
    },

    resize() {
      if (term && element.offsetParent !== null) fit.fit();
    },

    /** Close the session but keep the scrollback, so reopening is cheap. */
    disconnect() {
      socket?.close();
      socket = null;
    },
  };
}
