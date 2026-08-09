// The SQL editor: CodeMirror in whichever dialect the active source speaks,
// with autocompletion fed from the schema the explorer has loaded so far.

import { cteColumns, mask as maskLiterals, referencedTables } from './cte.js';
import { dialectForMime, qualifyLoosely } from './dialect.js';
import { importAt } from './federation.js';
import { targetAt } from './target.js';

/**
 * A declaration line inside a `DEFINE` block: its keyword, name, `=` and value.
 */
const DECLARATION = /^[ \t]*(ATTACH|IMPORT|FILES|EXCEL)[ \t]+(\w*)[ \t]*(=?)/i;

/**
 * Completions inside a `DEFINE` block, or null when the cursor is not in one.
 *
 * The whole reason the declarations moved out of comments: after `ATTACH pg =`
 * the only words that mean anything are the sources you have registered, and a
 * comment could never have said so. Without this the SQL hint answers with
 * whatever keyword happens to share a prefix.
 */
function declarationCompletions(cm, sources) {
  const cursor = cm.getCursor();
  const text = cm.getValue();
  // Only above EVALUATE: below it the buffer is ordinary SQL and the ordinary
  // completions are the right ones.
  const evaluate = text.search(/^[ \t]*EVALUATE\b/im);
  if (evaluate === -1) return null;
  const before = text.slice(0, cm.indexFromPos(cursor));
  if (before.length > evaluate) return null;

  const line = cm.getLine(cursor.line) ?? '';
  const match = DECLARATION.exec(line);
  if (!match || !match[3]) return null;

  // The value starts after the `=`; complete the source id in it, and leave a
  // `/database` or a glob alone.
  const start = line.indexOf('=', match[1].length) + 1;
  const value = line.slice(start);
  const offset = value.length - value.trimStart().length;
  const typed = line.slice(start + offset, cursor.ch);
  if (typed.includes('/') || cursor.ch < start + offset) return null;

  return {
    list: sources.filter((name) => name.toLowerCase().startsWith(typed.toLowerCase())),
    from: CodeMirror.Pos(cursor.line, start + offset),
    to: CodeMirror.Pos(cursor.line, Math.max(cursor.ch, start + offset)),
  };
}

/**
 * Completions for the target of a `TARGET`, or null when that is not what is
 * being typed.
 *
 * Without this the SQL hint answers `TARGET pg` with `PG_CONTEXT`,
 * `PG_DATATYPE_NAME` and the rest of PL/pgSQL's `PG_*` keywords — and since the
 * list pops up as you type, pressing Enter to move on accepts one. The directive
 * silently becomes `TARGET PG_CONTEXT` and stops working. After `TARGET` the only
 * words that mean anything are the sources you have registered, so those are the
 * only ones offered.
 */
function targetCompletions(cm, sources) {
  const cursor = cm.getCursor();
  const at = targetAt(cm.getValue().split('\n'), cursor.line, cursor.ch);
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

/** At most this many column suggestions: a wide schema has thousands. */
const MAX_COLUMNS = 40;

/**
 * One suggestion, drawn as a name and where it came from.
 *
 * Two lanes rather than one string, so `id` in a column and `id` in a table do not
 * look alike: the name is what you read, the origin is what tells you which of the
 * two you are about to accept. `displayText` is still set, because the addon uses
 * it for its own filtering and for the case where `render` is not called.
 */
function candidate(text, name, where, kind) {
  return {
    text,
    displayText: `${name} — ${where}`,
    className: `CodeMirror-hint-${kind}`,
    render(element) {
      const label = document.createElement('span');
      label.className = 'hint-name';
      label.textContent = name;
      const origin = document.createElement('span');
      origin.className = 'hint-where';
      origin.textContent = where;
      element.append(label, origin);
    },
  };
}

/** Words after which a *table* is what you are about to name. */
const TABLE_WORDS = /\b(?:from|join|into|update|using)\b/gi;
/** Words after which a *column* is. */
const COLUMN_WORDS =
  /\b(?:select|where|having|on|set|group|order|by|and|or|case|when|then|else|distinct)\b/gi;

function lastIndexOfAny(text, pattern) {
  pattern.lastIndex = 0;
  let at = -1;
  let found = pattern.exec(text);
  while (found) {
    at = found.index;
    found = pattern.exec(text);
  }
  return at;
}

/**
 * Whether a table is the likelier thing to be naming here.
 *
 * Both tables and columns are always offered — the clause only decides which goes
 * first, and that matters because the list pops up as you type and Enter accepts
 * whatever is highlighted.
 */
function wantsTable(cm) {
  const before = maskLiterals(cm.getRange({ line: 0, ch: 0 }, cm.getCursor()));
  return lastIndexOfAny(before, TABLE_WORDS) > lastIndexOfAny(before, COLUMN_WORDS);
}

/**
 * A test for "this table is in scope for column completion", or null when the
 * statement names no table and everything is.
 *
 * A reference may be written qualified or not — `sales.customer` or just
 * `customer` — so a key matches on either its whole name or its table part.
 */
function columnScope(cm, tables) {
  const cursor = cm.getCursor();
  const offset = cm.indexFromPos(cursor);
  const { names } = referencedTables(cm.getValue(), offset);
  if (names.size === 0) return null;

  const wanted = new Set([...names].map((name) => name.toLowerCase()));
  return (qualified) => {
    const lower = qualified.toLowerCase();
    if (wanted.has(lower)) return true;
    const cut = lower.indexOf('.');
    return cut !== -1 && wanted.has(lower.slice(cut + 1));
  };
}

/**
 * Candidates the SQL hint will not offer: schemas, tables found by their own name
 * rather than by the schema in front of it, and **columns**.
 *
 * The addon matches a candidate from its first character, and its candidates are
 * the *qualified* names — so `customer` matches nothing at all, and you have to
 * remember `sales` before it will help you find `sales.customer`. That is
 * backwards: the table name is the part you know. Columns it offers only after a
 * `table.`, which means the one thing you type most often is the one thing it
 * cannot help with.
 *
 * All of this only while the word being typed has no dot in it — after a dot the
 * addon already knows what to do.
 */
function byBareName(cm, result, tables, inScope, aliases) {
  const typed = cm.getRange(result.from, result.to);
  if (typed.includes('.') || typed.includes('"') || typed.includes('`')) {
    return { tables: [], columns: [], shadowed: new Set() };
  }

  const prefix = typed.toLowerCase();
  const already = new Set(
    result.list.map((item) => (typeof item === 'string' ? item : item.text)),
  );

  const schemas = new Set();
  const matched = [];
  /** Texts whose plain-string version the addon should no longer offer. */
  const shadowed = new Set();
  /** column name → the qualified tables holding one. */
  const columns = new Map();

  for (const [qualified, names] of Object.entries(tables)) {
    const cut = qualified.indexOf('.');
    if (cut !== -1) {
      const schema = qualified.slice(0, cut);
      const table = qualified.slice(cut + 1);
      if (schema.toLowerCase().startsWith(prefix)) schemas.add(schema);
      if (table.toLowerCase().startsWith(prefix) && !already.has(qualified)) {
        // The table first, since that is what you were looking for.
        matched.push(candidate(qualified, table, schema, 'table'));
      }
    } else if (qualified.toLowerCase().startsWith(prefix)) {
      // An unqualified entry is a CTE — or, in a federated buffer, a name the
      // DEFINE block declared, which is a different thing and should not be
      // called a CTE. The addon offers it too, as a bare string with nothing to
      // say about it, so this one *replaces* that rather than stepping aside for
      // it: otherwise the row that wins is the one that says nothing at all.
      const what = aliases?.has(qualified) ? 'declared' : 'CTE';
      matched.push(candidate(qualified, qualified, what, 'table'));
      shadowed.add(qualified);
    }

    // Columns only from the tables this statement actually reads. Offering all 285
    // columns of a database when the query names two tables is a haystack, not a
    // hint. With nothing named yet there is nothing to narrow by, so everything is
    // offered — which is the state you are in while typing the select list first.
    if (inScope && !inScope(qualified)) continue;
    for (const name of names ?? []) {
      if (!name.toLowerCase().startsWith(prefix)) continue;
      if (!columns.has(name)) columns.set(name, []);
      columns.get(name).push(qualified);
    }
  }

  const schemaItems = [...schemas]
    .filter((schema) => !already.has(schema))
    .sort()
    .map((schema) => candidate(schema, schema, 'schema', 'table'));

  matched.sort((a, b) => a.text.localeCompare(b.text));

  // One entry per column *name*, not per occurrence: `id` in forty tables is one
  // completion and forty lines of noise.
  const columnItems = [...columns.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .slice(0, MAX_COLUMNS)
    .map(([name, holders]) =>
      candidate(
        name,
        name,
        holders.length === 1 ? holders[0] : `${holders.length} tables`,
        'column',
      ),
    )
    .filter((item) => !already.has(item.text));

  return { tables: [...schemaItems, ...matched], columns: columnItems, shadowed };
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
  const target = targetCompletions(cm, options.sources ?? []);
  if (target) return target;
  const declaring = declarationCompletions(cm, options.sources ?? []);
  if (declaring) return declaring;

  // Which schema is in scope here.
  //
  // Inside `IMPORT s = src AS ( … )` the SQL belongs to `src`, so it is that
  // source's tables that mean anything — offering the buffer's own aliases there
  // would be offering names the source has never heard of. Everywhere else it is
  // the buffer's own map.
  //
  // Only the *map* changes. Everything below — bare names, columns, quoting — runs
  // either way, and the first version of this returned the addon's answer straight
  // from here, which quietly gave up all of it: typing `na` inside the brackets
  // offered nothing, because the addon only ever matches a qualified name.
  const inside = importAt(cm.getValue(), cm.indexFromPos(cm.getCursor()));
  const native = inside ? options.inside?.[inside.alias] : null;

  // CTEs are read from the buffer *here* rather than kept in step on every
  // keystroke: the parse is cheap, it only matters while the list is open, and
  // nothing can go stale if there is nothing to invalidate. A declaration's own
  // SQL has no CTEs of the outer buffer's in scope.
  const schema = native ?? options.tables ?? {};
  const withCtes = native ? schema : { ...schema, ...cteColumns(cm.getValue(), schema) };

  // The addon resolves `alias.` against its own `tables`, so it has to see the
  // CTEs too or `c.` after `with c as (…)` offers nothing.
  const result = CodeMirror.hint.sql(cm, { ...options, tables: withCtes });
  if (!result?.list) return result;

  const dialect = dialectForMime(cm.getOption('mode'));
  const reserved = CodeMirror.resolveMode(cm.getOption('mode'))?.keywords;

  const extra = byBareName(cm, result, withCtes, columnScope(cm, withCtes), options.aliases);
  const kept = result.list.filter(
    (item) => !extra.shadowed.has(typeof item === 'string' ? item : item.text),
  );
  // Both are always offered; the clause decides which the first Enter accepts.
  result.list = wantsTable(cm)
    ? [...extra.tables, ...extra.columns, ...kept]
    : [...extra.columns, ...extra.tables, ...kept];

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
 * Teach every SQL mode that `TARGET` is a keyword.
 *
 * It is alkyon's own directive rather than any engine's, but it is the first
 * word of the buffer and it *acts* like a statement, so leaving it in plain text
 * made it read as a mistake. The mode keeps a reference to this object, so
 * adding to it reaches modes already created. `swap`, the former spelling, is
 * still highlighted because it is still accepted.
 */
for (const mime of ['text/x-sql', 'text/x-pgsql', 'text/x-mssql', 'text/x-mysql']) {
  // Tokens are lower-cased before the lookup, so the keys are lower-case.
  const keywords = CodeMirror.mimeModes[mime]?.keywords;
  if (keywords) {
    keywords.target = true;
    keywords.swap = true;
    // And the federated block, for the same reason twice over: these words are
    // the buffer's structure, and left in plain text they read as prose. This is
    // what a declaration block being *code* rather than a comment buys.
    for (const word of ['define', 'evaluate', 'attach', 'import', 'files', 'excel']) {
      keywords[word] = true;
    }
  }
}

export function createEditor(element, { onRun, onOpen, onSave, onSaveAs, onNew, onClose, onChange }) {
  // `tables` is what CodeMirror's sql-hint completes from: qualified table name
  // to column names. Table names are registered as soon as a database is
  // expanded; the columns fill in when a table is.
  let tables = {};
  // Registered source names, for completing a TARGET directive.
  let sources = [];
  // Names a federated buffer declared, which are neither tables nor CTEs.
  let aliases = new Set();
  // alias → that source's own schema, for completing the SQL inside its brackets.
  let inside = {};

  const editor = CodeMirror(element, {
    value: [
      '-- Ctrl+Enter to run. With text selected, only the selection runs.',
      '-- TARGET <source> or TARGET <source>.<database> retargets the editor.',
      'select 1 as answer;',
      '',
    ].join('\n'),
    mode: 'text/x-sql',
    theme: 'night-owl',
    lineNumbers: true,
    // Enter keeps the indentation of the line above instead of asking the SQL
    // mode where the line "should" go.
    //
    // The mode's answer is bracket depth plus one unit, which is not how anyone
    // writes SQL: clauses are aligned under each other by hand, and a `WHERE`
    // typed under a `FROM` inside `AS (` came out two columns further in every
    // time. Copying the previous line is both predictable and what was meant.
    smartIndent: false,
    // Four, because that is what the buffers people write already use — the
    // default two left Tab and the existing indentation disagreeing.
    indentUnit: 4,
    tabSize: 4,
    // A real tab character, not four spaces pretending to be one. Asked for, and
    // it is the honest reading of pressing Tab. Existing lines keep whatever they
    // were written with: Enter copies the characters above it rather than
    // reformatting them, so nothing already indented with spaces is disturbed.
    indentWithTabs: true,
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
    hintOptions: { tables, sources, aliases, inside, completeSingle: false, hint: quotingSqlHint },
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
    editor.setOption('hintOptions', {
      tables,
      sources,
      aliases,
      inside,
      completeSingle: false,
      hint: quotingSqlHint,
    });
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
     * Put the buffer into DuckDB mode, or take it out.
     *
     * The text is the only state that decides which engine runs — there is no flag
     * beside it — which is what makes the mode survive a save and a reopen.
     *
     * Turning it on wraps what is already there in `EVALUATE`, because that is
     * what the buffer was: the thing to return. Turning it off unwraps it, and a
     * `DEFINE` block is dropped with a confirmation the caller shows, since those
     * declarations are not recoverable from anywhere else.
     */
    setFederated(on) {
      const text = editor.getValue();
      const head = text.replace(/^(?:\s|--[^\n]*\n|\/\*[\s\S]*?\*\/)*/, '');
      const already = /^(?:DEFINE|EVALUATE)\b/i.test(head);

      if (on === already) return;
      if (on) {
        // Indented under EVALUATE, so it reads as the block it now is.
        const body = text
          .split('\n')
          .map((line) => (line.trim() ? `    ${line}` : line))
          .join('\n');
        editor.setValue(`EVALUATE\n${body}`);
      } else {
        // Everything from EVALUATE onwards is the query; the declarations above it
        // have no meaning outside DuckDB mode and go.
        const at = text.search(/^[ \t]*EVALUATE[ \t]*$/im);
        const body = at === -1 ? text : text.slice(text.indexOf('\n', at) + 1);
        editor.setValue(
          body
            .split('\n')
            .map((line) => line.replace(/^ {4}/, ''))
            .join('\n')
            .trimStart(),
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

    /** What a TARGET may name. Bare ids, plus the qualified key when it differs. */
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
     *
     * `declared` names the entries that came from a federated buffer's DEFINE
     * block, so the list can say what they are rather than calling them CTEs.
     */
    setSchema(map, declared = [], within = {}) {
      tables = map;
      aliases = new Set(declared);
      inside = within;
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
