import { api, runQuery } from './api.js';
import { createBuffers } from './buffers.js';
import { previewSql, quoteFor } from './dialect.js';
import { createEditor } from './editor.js';
import { createExplorer } from './explorer.js';
import { canSaveInPlace, download, openFiles, pickSaveTarget, writeFile } from './files.js';
import { ResultGrid } from './grid.js';
import { parseSwap } from './swap.js';
import { createTerminal } from './terminal.js';
import { createSearch } from './search.js';
import { createTheme } from './theme.js';
import { createWorkspace } from './workspace.js';

const $ = (id) => document.getElementById(id);

const state = {
  sources: [],
  source: null,
  database: null,
  running: null,
  /** id → { ok, detail }, filled in by the explorer's probes. */
  health: new Map(),
};

// ------------------------------------------------------------------- status

function status(text, isError = false) {
  $('status-text').textContent = text;
  $('status').classList.toggle('error', isError);
}

function detail(text) {
  $('status-detail').textContent = text;
}

// --------------------------------------------------------------- components

const grid = new ResultGrid($('grid'));

const editor = createEditor($('editor-pane'), {
  onRun: run,
  onOpen: () => openIntoTabs(),
  onSave: () => save(),
  onSaveAs: () => save({ as: true }),
  onNew: () => buffers.open(),
  onClose: () => buffers.close(buffers.active),
  // Keeps the tab's dirty marker honest without polling, and the mode badge in
  // step with the directive as it is typed.
  onChange: () => {
    buffers.refresh();
    paintMode();
  },
});

const terminal = createTerminal($('terminal'), { onStatus: status });

// ------------------------------------------------------------------ buffers

const buffers = createBuffers($('tab-bar'), {
  onActivate: (buffer) => {
    editor.setDoc(buffer.doc);
    editor.setDialect(state.source?.editor_mime);
    // A tab remembers what it was last run against. Captured first, because
    // `selectSource` will rewrite `buffer.target` as it goes.
    paintMode();
    const target = buffer.target;
    if (target) {
      selectSource(target.sourceKey).then(() => {
        if (target.database) selectDatabase(target.database);
      });
    }
  },
});

/** Record the active target on the active tab, so switching back restores it. */
function rememberTarget() {
  if (buffers.active && state.source) {
    buffers.active.target = { sourceKey: state.source.key, database: state.database };
  }
}

async function openIntoTabs() {
  try {
    const files = await openFiles();
    for (const file of files) buffers.open(file);
    if (files.length) status(`opened ${files.map((f) => f.name).join(', ')}`);
  } catch (e) {
    status(e.message, true);
  }
}

async function save({ as = false } = {}) {
  const buffer = buffers.active;
  if (!buffer) return;
  const text = editor.text();

  // A file that came from the open folder is written back through the server —
  // that is the only side that knows the real path.
  if (buffer.workspacePath && !as) {
    try {
      await api.writeWorkspaceFile(buffer.workspacePath, text);
      buffers.markSaved(buffer);
      status(`saved ${buffer.workspacePath}`);
    } catch (e) {
      status(`save failed: ${e.message}`, true);
    }
    return;
  }

  try {
    let handle = as ? null : buffer.handle;
    if (!handle) {
      handle = await pickSaveTarget(buffer.name);
      if (!handle) {
        // No picker at all: the best we can do is hand the file to the browser.
        if (!canSaveInPlace) {
          download(buffer.name, text);
          status(`downloaded ${buffer.name} — this browser cannot save in place`);
        }
        return;
      }
    }
    await writeFile(handle, text);
    buffers.markSaved(buffer, { name: handle.name, handle });
    status(`saved ${handle.name}`);
  } catch (e) {
    status(`save failed: ${e.message}`, true);
  }
}

$('new-tab').addEventListener('click', () => buffers.open());
$('open-file').addEventListener('click', () => openIntoTabs());
$('save-file').addEventListener('click', () => save());
$('save-as-file').addEventListener('click', () => save({ as: true }));

// The browser's own guard against losing work.
addEventListener('beforeunload', (event) => {
  if (buffers.anyDirty()) event.preventDefault();
});

// --------------------------------------------------------------- duckdb mode

/**
 * Mirrors `federation::program::is_federated` for the badge only — the server
 * decides which engine actually runs the buffer, and its rules are the ones under
 * test. Kept to the same rule: the directive must be in the leading comments.
 */
function looksFederated(text) {
  for (const line of text.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (!trimmed.startsWith('--')) return false;
    if (/^\s*@duckdb\b/.test(trimmed.slice(2))) return true;
  }
  return false;
}

/** Reflect the buffer's mode in the chrome. Called on every edit and tab switch. */
function paintMode() {
  const federated = looksFederated(editor.text());
  $('toggle-duckdb').setAttribute('aria-pressed', String(federated));
  document.body.classList.toggle('federated', federated);

  // In federated mode there is no single source: the imports name their own, so
  // these pickers do not apply.
  for (const id of ['source-select', 'database-select']) {
    $(id).disabled = federated;
    $(id).title = federated
      ? 'not used in DuckDB mode — each @import names its own source'
      : '';
  }
  // CodeMirror has no DuckDB mode; the generic SQL one is the closest fit.
  if (federated) editor.setDialect('text/x-sql');
  else editor.setDialect(state.source?.editor_mime);
}

$('toggle-duckdb').addEventListener('click', () => {
  editor.setFederated(!looksFederated(editor.text()));
  paintMode();
});

// ------------------------------------------------------------------- schema

/**
 * Load the active database's whole schema and hand it to the editor, so
 * completion offers columns before you have browsed to a single table.
 *
 * Fire-and-forget: it must never delay switching source.
 */
function loadSchema({ refresh = false } = {}) {
  const source = state.source;
  const database = state.database;
  if (!source || !database) return;

  api
    .schema(source.key, database, { refresh })
    .then((snapshot) => {
      // The user may have moved on while this was in flight.
      if (state.source?.key !== source.key || state.database !== database) return;
      editor.setSchema(
        Object.fromEntries(
          snapshot.tables.map((table) => [
            `${table.schema}.${table.name}`,
            table.columns.map((column) => column.name),
          ]),
        ),
      );
      detail(`${source.key} · ${database} — ${snapshot.columns.toLocaleString()} columns`);
      search.refreshCoverage();
    })
    .catch((e) => status(`schema: ${e.message}`, true));
}

const search = createSearch(
  {
    input: $('search-input'),
    results: $('search-results'),
    coverage: $('search-coverage'),
    indexAll: $('index-all'),
  },
  {
    onStatus: status,
    onPick: async (hit) => {
      await selectSource(hit.source);
      selectDatabase(hit.database);
      editor.insert(qualify(hit.schema, hit.table));
      editor.focus();
    },
  },
);

/** Quote per the active dialect, the same way the explorer does. */
function qualify(schema, table) {
  const quote = (name) => quoteFor(state.source?.dialect, name);
  return `${quote(schema)}.${quote(table)}`;
}

$('index-all').addEventListener('click', async () => {
  const button = $('index-all');
  button.disabled = true;
  const label = button.textContent;
  let done = 0;
  const failures = [];

  // Sequential on purpose: indexing every server at once is a poor way to treat
  // production.
  for (const source of state.sources) {
    button.textContent = `Indexing ${done + 1}/${state.sources.length}…`;
    try {
      await api.schema(source.key, source.database);
      done += 1;
    } catch (e) {
      failures.push(`${source.key}: ${e.message}`);
    }
  }

  button.disabled = false;
  button.textContent = label;
  await search.refreshCoverage();
  await search.rerun();
  status(
    failures.length
      ? `indexed ${done}, failed ${failures.length} — ${failures[0]}`
      : `indexed ${done} database(s)`,
    failures.length > 0,
  );
});

addEventListener('keydown', (event) => {
  // Chrome allows a page to take Ctrl+K, unlike Ctrl+N or Ctrl+W.
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    search.focus();
  }
});

// ---------------------------------------------------------------- workspace

const workspace = createWorkspace($('workspace'), {
  onStatus: status,
  onRootChange: (root) => {
    const line = $('workspace-root');
    // Keep the tail — the last couple of directories are what identify a project.
    line.textContent = root && root.length > 44 ? `…${root.slice(-43)}` : (root ?? '');
    line.title = root ?? '';
    line.hidden = !root;
    $('close-folder').hidden = !root;
  },
  onOpenFile: async (file) => {
    try {
      const { text } = await api.readWorkspaceFile(file.path);
      buffers.open({ name: file.name, text, workspacePath: file.path });
      status(`opened ${file.path}`);
    } catch (e) {
      status(e.message, true);
    }
  },
});

const folderDialog = $('folder-dialog');
const folderForm = $('folder-form');

function folderNote(text, kind) {
  const element = $('folder-note');
  element.textContent = text ?? '';
  element.className = kind ? `note ${kind}` : 'note';
  element.hidden = !text;
}

$('open-folder').addEventListener('click', () => {
  folderForm.reset();
  folderNote();
  folderForm.elements.path.value = workspace.root ?? '';
  folderDialog.showModal();
});
$('folder-cancel').addEventListener('click', () => folderDialog.close());

folderForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('folder-open');
  button.disabled = true;
  folderNote();
  try {
    const state = await workspace.open(folderForm.elements.path.value.trim());
    folderDialog.close();
    // The folder brings its own sources with it.
    await loadSources();
    status(`opened ${state.root} — ${state.files.length} .sql file(s)`);
    // The working directory is fixed when the shell is spawned, so an open
    // terminal has to be restarted to land in the new folder.
    terminal.restart();
  } catch (e) {
    folderNote(e.message, 'bad');
  } finally {
    button.disabled = false;
  }
});

$('close-folder').addEventListener('click', async () => {
  try {
    await workspace.close();
    // Its project sources go with it.
    await loadSources();
    status('folder closed');
    terminal.restart();
  } catch (e) {
    status(e.message, true);
  }
});

const theme = createTheme({
  onResolve: (resolved) => {
    editor.setTheme(theme.editorTheme());
    terminal.setTheme(resolved);
    $('theme-toggle').textContent = theme.label();
  },
});

const explorer = createExplorer($('explorer'), {
  onStatus: status,
  onProbe: (source, ok, detail) => {
    state.health.set(source.key, { ok, detail });
    if (source.key === state.source?.key) paintSourceStatus();
  },
  onSelectSource: (source) => selectSource(source.key),
  // Awaited so the database list is in place before we pick from it — otherwise
  // a slow `/databases` response resets the choice a moment later.
  onSelectDatabase: async (source, db) => {
    await selectSource(source.key);
    selectDatabase(db);
  },
  onInsert: (qualified) => editor.insert(qualified),
  // Clicking a table peeks at it. The buffer is left alone — this is a look, not
  // an edit, so whatever you were writing survives.
  onPreview: (source, qualified) => {
    if (state.busy || state.running) return;
    stream(previewSql(source.dialect, qualified));
  },
  onTables: (source, db, tables) => {
    if (source.key !== state.source?.key) return;
    editor.addTables(tables.map((t) => `${t.schema}.${t.name}`));
  },
  onColumns: (source, db, qualified, columns) => {
    if (source.key !== state.source?.key) return;
    editor.addColumns(qualified, columns.map((c) => c.name));
  },
});

// ---------------------------------------------------------- source handling

/** Mirror the active source's probe result next to the header picker. */
function paintSourceStatus() {
  const dot = $('source-status');
  const health = state.health.get(state.source?.key);
  dot.className = health ? (health.ok ? 'dot ok' : 'dot down') : 'dot';
  dot.title = health?.detail ?? 'connection state unknown';
}

async function selectSource(key) {
  if (state.source?.key === key) return;
  state.source = state.sources.find((source) => source.key === key) ?? null;
  $('source-select').value = key ?? '';
  editor.setDialect(state.source?.editor_mime);
  editor.clearSchema();
  paintSourceStatus();
  await populateDatabases();
  rememberTarget();
}

function selectDatabase(db) {
  if (state.database === db) return;
  state.database = db;
  const select = $('database-select');
  // A database named by SWAP may not be in the fetched list — if the server
  // never listed it, let the engine be the one to complain.
  if (db && ![...select.options].some((option) => option.value === db)) {
    const option = document.createElement('option');
    option.value = db;
    option.textContent = db;
    select.append(option);
  }
  select.value = db ?? '';
  editor.clearSchema();
  rememberTarget();
  loadSchema();
}

/**
 * The database list is best-effort: a source whose server is unreachable still
 * has to be selectable, so it falls back to the source's own database.
 */
async function populateDatabases() {
  const select = $('database-select');
  if (!state.source) {
    select.replaceChildren();
    state.database = null;
    return;
  }

  const fallback = [state.source.database];
  let databases = fallback;
  try {
    databases = await api.databases(state.source.key);
  } catch (e) {
    status(`${state.source.key}: ${e.message}`, true);
  }

  select.replaceChildren(...databases.map((db) => {
    const option = document.createElement('option');
    option.value = db;
    option.textContent = db;
    return option;
  }));

  state.database = databases.includes(state.source.database)
    ? state.source.database
    : databases[0] ?? null;
  select.value = state.database ?? '';
  loadSchema();
}

async function loadSources({ keep = true } = {}) {
  state.sources = await explorer.refresh();

  const select = $('source-select');
  // Two scopes may define the same bare id, so the option value is the qualified
  // key and the label says which one it is.
  const ambiguous = new Set(
    state.sources
      .map((source) => source.id)
      .filter((id, index, all) => all.indexOf(id) !== index),
  );
  select.replaceChildren(...state.sources.map((source) => {
    const option = document.createElement('option');
    option.value = source.key;
    option.textContent =
      source.scope === 'project' || ambiguous.has(source.id)
        ? `${source.id} (${source.scope})`
        : source.id;
    return option;
  }));

  const previous = keep ? state.source?.key : null;
  const next = state.sources.find((source) => source.key === previous) ?? state.sources[0];
  state.source = null;
  if (next) {
    await selectSource(next.key);
  } else {
    $('database-select').replaceChildren();
    status('no sources registered — use + in the explorer to add one');
  }
}

// --------------------------------------------------------------------- swap

/**
 * Apply any leading `SWAP <source>[.<database>]` directives. Returns the SQL left
 * to run, or `null` when a directive named something unknown.
 */
/**
 * A SWAP token may be a qualified `scope:id` or a bare id, when only one scope
 * defines it. Returns the key, or `null`.
 */
function resolveSourceKey(token) {
  if (state.sources.some((source) => source.key === token)) return token;
  const matches = state.sources.filter((source) => source.id === token);
  return matches.length === 1 ? matches[0].key : null;
}

async function applySwaps(sql) {
  const parsed = parseSwap(sql, (token) => resolveSourceKey(token) !== null);

  if (parsed.unknown) {
    const known = state.sources.map((source) => source.key).join(', ') || 'none registered';
    status(`SWAP: cannot resolve \`${parsed.unknown}\` — known: ${known}`, true);
    return null;
  }

  let swapped = null;
  for (const target of parsed.targets) {
    const key = resolveSourceKey(target.id);
    await selectSource(key);
    if (target.database) selectDatabase(target.database);
    swapped = `${key} · ${state.database ?? ''}`;
  }

  if (swapped) detail(swapped);
  return { sql: parsed.sql, swapped };
}

// -------------------------------------------------------------------- query

async function run() {
  if (state.busy || state.running) return;
  state.busy = true;
  try {
    await execute();
  } finally {
    state.busy = false;
  }
}

async function execute() {
  const applied = await applySwaps(editor.sql().trim());
  if (!applied) return;

  const { sql, swapped } = applied;
  if (!sql) {
    if (swapped) status(`swapped to ${swapped}`);
    else status('nothing to run', true);
    return;
  }
  stream(sql);
}

/** The row cap the header picker is set to; `0` means the user asked for none. */
function maxRows() {
  return Number($('max-rows').value);
}

/** Send one statement to the active source and render what comes back. */
function stream(sql) {
  if (!state.source) return status('no source selected', true);

  let rows = 0;
  let sawColumns = false;
  $('run').disabled = true;
  $('cancel').disabled = false;
  status('running…');
  detail(`${state.source.key} · ${state.database ?? state.source.database}`);

  state.running = runQuery(
    {
      sourceId: state.source.key,
      database: state.database,
      sql,
      maxRows: maxRows(),
    },
    {
      columns: ({ columns }) => {
        sawColumns = true;
        grid.reset(columns);
      },
      rows: (message) => {
        rows += message.rows.length;
        grid.append(message.rows);
        status(`${rows.toLocaleString()} rows…`);
      },
      affected: ({ rows_affected }) => {
        if (!sawColumns) grid.message(`${rows_affected} row(s) affected`);
        status(`${rows_affected} row(s) affected`);
      },
      end: ({ rows: total, elapsed_ms, truncated }) => {
        if (!sawColumns && total === 0) grid.message('statement completed, no rows returned');
        else grid.finish();
        // Say plainly that there was more, rather than letting a capped result
        // read as the whole answer.
        status(
          truncated
            ? `first ${total.toLocaleString()} row(s) in ${elapsed_ms} ms — stopped at the row limit, there are more`
            : `${total.toLocaleString()} row(s) in ${elapsed_ms} ms`,
        );
      },
      cancelled: () => status('cancelled'),
      error: ({ message }) => {
        grid.message(message);
        status(message, true);
      },
      closed: () => {
        state.running = null;
        $('run').disabled = false;
        $('cancel').disabled = true;
      },
    },
  );
}

$('run').addEventListener('click', run);
$('cancel').addEventListener('click', () => state.running?.cancel());
$('source-select').addEventListener('change', (e) => selectSource(e.target.value));
$('database-select').addEventListener('change', (e) => selectDatabase(e.target.value));
$('refresh-explorer').addEventListener('click', () => loadSources());

// ----------------------------------------------------------------- terminal

let terminalOpen = false;

function toggleTerminal(open = !terminalOpen) {
  terminalOpen = open;
  $('terminal-pane').hidden = !open;
  $('split-terminal').hidden = !open;
  $('toggle-terminal').setAttribute('aria-pressed', String(open));
  document.body.style.setProperty('--terminal', open ? '32vh' : '0px');
  // Closing drops the shell rather than leaving a process behind a hidden pane.
  if (open) terminal.open();
  else terminal.disconnect();
  editor.refresh();
}

$('toggle-terminal').addEventListener('click', () => toggleTerminal());
$('terminal-close').addEventListener('click', () => toggleTerminal(false));
$('shell-select').addEventListener('change', (e) => terminal.setShell(e.target.value));

/** Fill the shell picker from what the server actually found on PATH. */
async function loadShells() {
  const select = $('shell-select');
  try {
    const { shells, default: preferred, enabled } = await api.shells();
    select.replaceChildren(...shells.map((shell) => {
      const option = document.createElement('option');
      option.value = shell.name;
      option.textContent = shell.name;
      option.title = shell.path;
      return option;
    }));
    if (preferred) select.value = preferred;
    $('toggle-terminal').disabled = !enabled || shells.length === 0;
    if (!enabled) {
      $('toggle-terminal').title = 'disabled: alkyon is not bound to loopback';
    } else if (shells.length === 0) {
      $('toggle-terminal').title = 'no shell found on PATH';
    }
  } catch {
    $('toggle-terminal').disabled = true;
  }
}

// ------------------------------------------------------------------- theme

$('theme-toggle').addEventListener('click', () => {
  theme.cycle();
  editor.refresh();
});

// ---------------------------------------------------------------- splitters

/**
 * Drag a splitter to resize the pane before it, writing the result to a CSS
 * custom property on <body> that the grid templates read.
 */
function makeSplitter(handle, { property, axis, container, min = 120, max = 0.85 }) {
  handle.addEventListener('pointerdown', (event) => {
    event.preventDefault();
    handle.classList.add('dragging');
    handle.setPointerCapture(event.pointerId);

    const box = container.getBoundingClientRect();

    const move = (move_) => {
      const size = axis === 'x' ? move_.clientX - box.left : move_.clientY - box.top;
      const limit = (axis === 'x' ? box.width : box.height) * max;
      document.body.style.setProperty(
        property,
        `${Math.round(Math.min(Math.max(size, min), limit))}px`,
      );
      editor.refresh();
      terminal.resize();
    };

    const up = () => {
      handle.classList.remove('dragging');
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
    };

    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
  });
}

makeSplitter($('split-sidebar'), {
  property: '--sidebar',
  axis: 'x',
  container: document.querySelector('main'),
});
makeSplitter($('split-editor'), {
  property: '--editor',
  axis: 'y',
  container: $('work'),
  min: 60,
});

// ------------------------------------------------------------ source dialog

const dialog = $('source-dialog');
const form = $('source-form');

/** What each engine connects to when the Database field is left empty. */
const DEFAULT_DATABASE = {
  ms_sql: 'master',
  my_sql: 'information_schema',
  postgres: 'postgres',
};

/** Show only the fields the selected engine and auth method actually use. */
function syncDialogFields() {
  const kind = form.elements.kind.value;
  const method = form.elements.method.value;
  // A folder or file is reached through the filesystem: no host, no port, no
  // encryption and nothing to authenticate.
  const server = kind !== 'files';

  for (const element of form.querySelectorAll('.server-only')) element.hidden = !server;
  for (const element of form.querySelectorAll('.files-only')) element.hidden = server;
  for (const label of form.querySelectorAll('.mssql-only')) {
    label.hidden = kind !== 'ms_sql';
  }
  // Say what leaving it empty will actually connect to.
  $('database-field').placeholder = `optional — defaults to ${DEFAULT_DATABASE[kind] ?? 'the engine default'}`;
  for (const label of form.querySelectorAll('[class^="auth-"]')) {
    label.hidden = !server || !label.classList.contains(`auth-${method}`);
  }
  // Windows integrated authentication is a SQL Server concept.
  const integrated = form.querySelector('option[value="integrated"]');
  integrated.disabled = kind !== 'ms_sql';
  if (integrated.disabled && method === 'integrated') {
    form.elements.method.value = 'password';
    syncDialogFields();
  }
}

form.elements.kind.addEventListener('change', syncDialogFields);
form.elements.method.addEventListener('change', syncDialogFields);

$('add-source').addEventListener('click', () => {
  form.reset();
  note();
  // A project source has nowhere to live without an open folder.
  const project = form.querySelector('option[value="project"]');
  project.disabled = !workspace.root;
  project.textContent = workspace.root
    ? 'This folder — .alkyon/sources.json, committable'
    : 'This folder — needs an open folder';
  syncDialogFields();
  dialog.showModal();
});

$('source-cancel').addEventListener('click', () => dialog.close());

/** The one line of feedback under the form. No argument clears it. */
function note(text, kind) {
  const element = $('source-note');
  element.textContent = text ?? '';
  element.className = kind ? `note ${kind}` : 'note';
  element.hidden = !text;
}

/** Read the form into the `POST /sources` shape. */
function readForm() {
  const data = new FormData(form);
  const kind = data.get('kind');

  // A folder source sends a path and nothing else; the fields the form still
  // holds for the other engines would only be noise on the wire.
  if (kind === 'files') {
    return {
      id: data.get('id').trim(),
      scope: data.get('scope'),
      kind,
      path: data.get('path').trim(),
      auth: { method: 'none' },
    };
  }

  const method = data.get('method');
  const auth = { method };
  if (method === 'password') {
    auth.username = data.get('username');
    auth.password = data.get('password');
  } else if (method === 'aad_token') {
    auth.token = data.get('token');
  }

  const config = {
    id: data.get('id').trim(),
    scope: data.get('scope'),
    kind,
    host: data.get('host').trim(),
    tls: data.get('tls'),
    auth,
  };
  // Omitted entirely rather than sent empty: the backend picks the engine's
  // always-present database (`postgres`, `master`, `information_schema`).
  const database = data.get('database').trim();
  if (database) config.database = database;
  const port = data.get('port');
  if (port) config.port = Number(port);
  const instance = data.get('instance');
  if (instance && kind === 'ms_sql') config.instance = instance.trim();

  return config;
}

/** Run `action` with the button showing progress and the outcome in the note. */
async function withFeedback(button, busyLabel, action) {
  const label = button.textContent;
  button.disabled = true;
  button.textContent = busyLabel;
  note();
  try {
    await action();
  } catch (e) {
    note(e.message, 'bad');
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}

$('source-test').addEventListener('click', () => {
  const config = readForm();
  // Neither field can carry `required`: whichever one the engine does not use is
  // hidden, and a hidden required field makes the form unsubmittable outright.
  // So the one that matters is checked here.
  if (config.kind === 'files' && !config.path) return note('give a path to test', 'bad');
  if (config.kind !== 'files' && !config.host) return note('give a host to test', 'bad');

  withFeedback($('source-test'), 'Testing…', async () => {
    const result = await api.testConnection(config);
    note(
      config.kind === 'files'
        ? `Readable in ${result.latency_ms} ms.`
        : `Connected to ${result.database} in ${result.latency_ms} ms.`,
      'good',
    );
  });
});

form.addEventListener('submit', (event) => {
  event.preventDefault();
  const config = readForm();
  withFeedback($('source-save'), 'Connecting…', async () => {
    const summary = await api.addSource(config);
    dialog.close();
    await loadSources({ keep: false });
    await selectSource(summary.key);
    status(`registered ${summary.key}`);
  });
});

// ------------------------------------------------------------------ startup

theme.apply();

// The first tab adopts whatever the editor was seeded with, so the hint text in
// it survives.
buffers.open({ text: editor.text() });
paintMode();
if (!canSaveInPlace) {
  $('save-file').title = 'This browser cannot save in place — Save downloads the file';
}

try {
  const health = await api.health();
  $('build').textContent = `v${health.version} · vault: ${health.vault}`;
} catch {
  status('cannot reach the alkyon backend', true);
}

await Promise.all([loadSources(), loadShells(), workspace.refresh()]);
editor.focus();
