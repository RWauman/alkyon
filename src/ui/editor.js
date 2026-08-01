// The SQL editor: CodeMirror in whichever dialect the active source speaks,
// with autocompletion fed from the schema the explorer has loaded so far.

export function createEditor(element, { onRun, onOpen, onSave, onSaveAs, onNew, onClose, onChange }) {
  // `tables` is what CodeMirror's sql-hint completes from: qualified table name
  // to column names. Table names are registered as soon as a database is
  // expanded; the columns fill in when a table is.
  let tables = {};

  const editor = CodeMirror(element, {
    value: [
      '-- Ctrl+Enter to run. With text selected, only the selection runs.',
      '-- SWAP <source> or SWAP <source>.<database> retargets the editor.',
      'select 1 as answer;',
      '',
    ].join('\n'),
    mode: 'text/x-sql',
    theme: 'night-owl',
    lineNumbers: true,
    matchBrackets: true,
    autoCloseBrackets: true,
    lineWrapping: true,
    styleActiveLine: true,
    // Dims the other occurrences of whatever is selected, which is what makes
    // Ctrl+Shift+L predictable before you press it.
    highlightSelectionMatches: { minChars: 2, showToken: false, annotateScrollbar: true },
    extraKeys: {
      'Ctrl-Enter': onRun,
      'Cmd-Enter': onRun,
      F5: onRun,

      'Ctrl-Space': 'autocomplete',
      'Ctrl-/': 'toggleComment',
      'Cmd-/': 'toggleComment',

      // Search, from the vendored addons.
      'Ctrl-F': 'find',
      'Cmd-F': 'find',
      'Ctrl-G': 'findNext',
      F3: 'findNext',
      'Shift-F3': 'findPrev',
      'Ctrl-H': 'replace',
      // CodeMirror normalises a key name to Alt-Ctrl-Shift-Cmd order and then
      // looks it up verbatim, so Shift must come first or the binding is dead.
      'Shift-Ctrl-H': 'replaceAll',
      'Alt-G': 'jumpToLine',
      Esc: 'clearSearch',

      // VS Code's multi-cursor pair, from the sublime keymap's commands. The
      // keymap itself is not installed — only the commands it registers.
      'Shift-Ctrl-L': 'findAllUnder',
      'Shift-Cmd-L': 'findAllUnder',
      'Ctrl-D': 'selectNextOccurrence',
      'Cmd-D': 'selectNextOccurrence',

      'Ctrl-S': onSave,
      'Cmd-S': onSave,
      'Shift-Ctrl-S': onSaveAs,
      'Shift-Cmd-S': onSaveAs,
      'Ctrl-O': onOpen,
      'Cmd-O': onOpen,
      // Not Ctrl-N/Ctrl-W: those are browser-level shortcuts a page cannot
      // intercept, and Ctrl-W would have closed the browser tab outright.
      'Alt-N': onNew,
      'Alt-W': onClose,
    },
    hintOptions: { tables, completeSingle: false },
  });

  // Pop the completion list up while typing a word or just after a dot, rather
  // than only on Ctrl+Space.
  editor.on('inputRead', (_, change) => {
    if (change.origin !== '+input') return;
    if (!/^[\w.]$/.test(change.text[0] ?? '')) return;
    if (editor.state.completionActive) return;
    CodeMirror.commands.autocomplete(editor);
  });

  if (onChange) editor.on('changes', () => onChange());

  function refreshHints() {
    editor.setOption('hintOptions', { tables, completeSingle: false });
  }

  return {
    editor,

    /** Swap in another buffer's document, keeping its history and cursor. */
    setDoc(doc) {
      editor.swapDoc(doc);
      editor.focus();
    },

    /** `mime` comes from the source's `editor_mime`. */
    setDialect(mime) {
      editor.setOption('mode', mime ?? 'text/x-sql');
    },

    /** `night-owl` or `light-owl`; both are defined in theme.css. */
    setTheme(name) {
      editor.setOption('theme', name);
    },

    /**
     * Add or remove the `-- @duckdb` line. The directive is the only state that
     * decides which engine runs the buffer, so the button edits the text rather
     * than holding a flag of its own — that is what makes it survive a save.
     */
    setFederated(on) {
      const DIRECTIVE = '-- @duckdb';
      const lines = editor.getValue().split('\n');
      const at = lines.findIndex((line) => /^\s*--\s*@duckdb\b/.test(line));

      if (on && at === -1) {
        editor.replaceRange(`${DIRECTIVE}\n`, { line: 0, ch: 0 });
      } else if (!on && at !== -1) {
        editor.replaceRange(
          '',
          { line: at, ch: 0 },
          { line: at + 1, ch: 0 },
        );
      }
      editor.focus();
    },

    /** The statement to run: the selection if there is one, else the buffer. */
    sql() {
      return editor.somethingSelected() ? editor.getSelection() : editor.getValue();
    },

    text() {
      return editor.getValue();
    },

    /** Forget the schema — the active source or database changed. */
    clearSchema() {
      tables = {};
      refreshHints();
    },

    /**
     * Replace the whole completion map at once. Feeding a full snapshot through
     * `addColumns` would call `setOption` once per table.
     */
    setSchema(map) {
      tables = map;
      refreshHints();
    },

    addTables(names) {
      for (const name of names) tables[name] ??= [];
      refreshHints();
    },

    addColumns(qualified, names) {
      tables[qualified] = names;
      refreshHints();
    },

    insert(text) {
      editor.replaceSelection(text);
      editor.focus();
    },

    focus() {
      editor.focus();
    },

    refresh() {
      editor.refresh();
    },
  };
}
