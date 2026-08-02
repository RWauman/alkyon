// The SQL editor: CodeMirror in whichever dialect the active source speaks,
// with autocompletion fed from the schema the explorer has loaded so far.

import { dialectForMime, qualifyLoosely } from './dialect.js';
import { swapTargetAt } from './swap.js';

/**
 * Completions for the target of a `SWAP`, or null when that is not what is being
 * typed.
 *
 * Without this the SQL hint answers `SWAP pg` with `PG_CONTEXT`,
 * `PG_DATATYPE_NAME` and the rest of PL/pgSQL's `PG_*` keywords — and since the
 * list pops up as you type, pressing Enter to move on accepts one. The directive
 * silently becomes `SWAP PG_CONTEXT` and stops working. After `SWAP` the only
 * words that mean anything are the sources you have registered, so those are the
 * only ones offered.
 */
function swapCompletions(cm, sources) {
  const cursor = cm.getCursor();
  const at = swapTargetAt(cm.getValue().split('\n'), cursor.line, cursor.ch);
  if (!at) return null;

  const typed = at.token.toLowerCase();
  const list = sources.filter((name) => name.toLowerCase().startsWith(typed));
  return {
    list,
    from: CodeMirror.Pos(cursor.line, at.start),
    // Replace the whole target, not just what is left of the cursor, so
    // completing halfway through a name does not leave its tail behind.
    to: CodeMirror.Pos(cursor.line, at.end),
  };
}

/**
 * `CodeMirror.hint.sql`, with the names it suggests made valid.
 *
 * The addon can quote a candidate — it has the code for it — but only once you
 * have already typed a quote yourself, which is no help when you do not know a
 * name needs one. A folder called `2022` is a schema called `2022`, and the
 * completion cheerfully offered `2022.trips`, which is a syntax error.
 *
 * Matching still happens against the bare name inside the addon, so typing
 * `2022` finds it; only the text that gets inserted changes. The list shows the
 * bare name too, because that is the one you recognise.
 */
function quotingSqlHint(cm, options) {
  const swap = swapCompletions(cm, options.sources ?? []);
  if (swap) return swap;

  const result = CodeMirror.hint.sql(cm, options);
  if (!result?.list) return result;

  const dialect = dialectForMime(cm.getOption('mode'));
  const reserved = CodeMirror.resolveMode(cm.getOption('mode'))?.keywords;

  result.list = result.list.map((item) => {
    const bare = typeof item === 'string' ? item : item.text;
    if (typeof bare !== 'string') return item;
    // A keyword is offered *as* a keyword. Quoting `SELECT` would turn the
    // completion into a column called "SELECT", which is the opposite of help.
    if (typeof item !== 'string' && /hint-keyword/.test(item.className ?? '')) return item;

    const quoted = qualifyLoosely(dialect, bare, reserved);
    if (quoted === bare) return item;
    return typeof item === 'string'
      ? { text: quoted, displayText: bare }
      : { ...item, text: quoted, displayText: item.displayText ?? bare };
  });
  return result;
}

/**
 * Teach every SQL mode that `SWAP` is a keyword.
 *
 * It is alkyon's own directive rather than any engine's, but it is the first
 * word of the buffer and it *acts* like a statement, so leaving it in plain text
 * made it read as a mistake. The mode keeps a reference to this object, so
 * adding to it reaches modes already created.
 */
for (const mime of ['text/x-sql', 'text/x-pgsql', 'text/x-mssql', 'text/x-mysql']) {
  // Tokens are lower-cased before the lookup, so the key is `swap`.
  const keywords = CodeMirror.mimeModes[mime]?.keywords;
  if (keywords) keywords.swap = true;
}

export function createEditor(element, { onRun, onOpen, onSave, onSaveAs, onNew, onClose, onChange }) {
  // `tables` is what CodeMirror's sql-hint completes from: qualified table name
  // to column names. Table names are registered as soon as a database is
  // expanded; the columns fill in when a table is.
  let tables = {};
  // Registered source names, for completing a SWAP target.
  let sources = [];

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
    hintOptions: { tables, sources, completeSingle: false, hint: quotingSqlHint },
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
    editor.setOption('hintOptions', { tables, sources, completeSingle: false, hint: quotingSqlHint });
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

    /** What a SWAP may name. Bare ids, plus the qualified key when it differs. */
    setSources(names) {
      sources = names;
      refreshHints();
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
