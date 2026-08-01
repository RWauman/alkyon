// Night Owl / Light Owl, with `auto` following the OS.
//
// The mode is stored; the *resolved* theme is what CodeMirror needs, since it
// takes a theme name rather than reading CSS variables.

const KEY = 'alkyon.theme';
const MODES = ['auto', 'light', 'dark'];
const LABELS = { auto: '◐ Auto', light: '☀ Light', dark: '☾ Dark' };

export function createTheme({ onResolve }) {
  const dark = matchMedia('(prefers-color-scheme: dark)');
  let mode = MODES.includes(localStorage.getItem(KEY)) ? localStorage.getItem(KEY) : 'auto';

  const resolved = () => (mode === 'auto' ? (dark.matches ? 'dark' : 'light') : mode);

  function apply() {
    // Leaving the attribute off in `auto` lets the media query in theme.css win.
    if (mode === 'auto') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = mode;
    onResolve(resolved());
  }

  dark.addEventListener('change', () => {
    if (mode === 'auto') apply();
  });

  return {
    apply,
    label: () => LABELS[mode],
    /** CodeMirror theme name for the resolved theme. */
    editorTheme: () => (resolved() === 'dark' ? 'night-owl' : 'light-owl'),
    cycle() {
      mode = MODES[(MODES.indexOf(mode) + 1) % MODES.length];
      localStorage.setItem(KEY, mode);
      apply();
    },
  };
}
