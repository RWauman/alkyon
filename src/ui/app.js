import { api, runQuery } from './api.js';
import { createBuffers } from './buffers.js';
import { previewSql, quoteFor } from './dialect.js';
import { createEditor } from './editor.js';
import { createExplorer } from './explorer.js';
import { createPanes } from './panes.js';
import { canSaveInPlace, download, openFiles, pickSaveTarget, writeFile } from './files.js';
import { ResultGrid } from './grid.js';
import { findCrossSource, unresolvedHeads } from './qualified.js';
import { parseTarget } from './target.js';
import { createTerminal } from './terminal.js';
import { createSearch } from './search.js';
import { createTheme } from './theme.js';
import { createWorkspace } from './workspace.js';

const $ = (id) => document.getElementById(id);

const state = {
  sources: [],
  source: null,
  database: null,
  /** The open result, kept for as long as its pages may still be read. */
  running: null,
  /** The on-screen result's columns, or null when there is no result. */
  columns: null,
  /** True while a page is on its way; what stops two queries overlapping. */
  inFlight: false,
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

// The filter's count goes in the detail slot rather than in the status line: the
// status line holds what the *query* did, which a filter has not changed.
const grid = new ResultGrid($('grid'), (text) => {
  if (text) detail(text);
  else if (state.source) detail(`${state.source.key} · ${state.database ?? state.source.database}`);
});

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

// Nothing outside the sidebar measures itself against it, so there is no resize
// hook to pass: the sections only rearrange each other.
const panes = createPanes($('explorer-pane'));

// ------------------------------------------------------------------ buffers

/**
 * The tab whose result is on screen.
 *
 * A result belongs to the tab that asked for it. Without that, looking at a
 * table wipes whatever you had just run — which is what made the preview
 * annoying enough to fix.
 */
let showing = null;

const buffers = createBuffers($('tab-bar'), {
  onActivate: (buffer) => {
    stashResult();
    showing = buffer;
    paintStart();

    // `null` means every tab is closed: the start screen has the pane.
    if (!buffer) {
      grid.destroy();
      state.running = null;
      state.columns = null;
      resetPaging();
      paintPaging();
      status('idle');
      detail('');
      return;
    }

    editor.setDoc(buffer.doc);
    editor.setDialect(state.source?.editor_mime);
    // A tab remembers what it was last run against. Captured first, because
    // `selectSource` will rewrite `buffer.target` as it goes.
    paintMode();
    restoreResult(buffer);

    const target = buffer.target;
    if (target) {
      selectSource(target.sourceKey).then(() => {
        if (target.database) selectDatabase(target.database);
      });
    }
  },
});

/** The start screen owns the editor pane whenever no tab is open. */
function paintStart() {
  const open = buffers.all.length > 0;
  $('editor-pane').hidden = !open;
  $('start').hidden = open;
  for (const id of ['run', 'save-file', 'save-as-file']) $(id).disabled = !open;
  editor.refresh();
}

/** Fill the start screen's list of folders opened before. */
function paintRecent(recent) {
  const list = $('start-recent');
  list.replaceChildren();
  if (!recent?.length) return;

  const heading = document.createElement('div');
  heading.className = 'heading';
  heading.textContent = 'Recent folders';
  list.append(heading);

  for (const path of recent) {
    const item = document.createElement('button');
    item.className = 'recent';
    // `direction: rtl` keeps the tail visible; the marks stop the browser from
    // reordering the leading drive letter along with it.
    item.textContent = `‪${path}‬`;
    item.title = path;
    item.addEventListener('click', () => openFolder(path));
    list.append(item);
  }
}

/** Park the on-screen result on the tab that owns it. */
function stashResult() {
  if (!showing) return;

  // A page still on its way belongs to a tab that is about to leave the screen.
  // Rather than let its rows arrive into someone else's grid, give it up — and
  // say so, instead of leaving a spinner parked on a tab nobody is watching.
  if (state.inFlight) {
    state.running?.cancel();
    setRunning(false);
    status('cancelled — the tab changed while it was loading');
  }
  showing.result = state.columns
    ? {
        columns: state.columns,
        pages: paging.pages,
        at: paging.at,
        first: paging.first,
        more: paging.more,
        // The open cursor travels with its tab, so turning a page still works
        // after you have been somewhere else and come back.
        running: state.running,
        status: $('status-text').textContent,
        detail: $('status-detail').textContent,
        error: $('status').classList.contains('error'),
      }
    : null;
}

/** Put a tab's own result back on screen, or clear the pane if it has none. */
function restoreResult(buffer) {
  const saved = buffer.result;
  state.running = saved?.running ?? null;
  state.columns = saved?.columns ?? null;
  resetPaging();

  if (!saved) {
    grid.destroy();
    status('idle');
    detail('');
    paintPaging();
    setRunning(false);
    return;
  }

  Object.assign(paging, {
    pages: saved.pages,
    at: saved.at,
    first: saved.first,
    more: saved.more,
  });
  grid.reset(saved.columns);
  grid.replace(saved.pages[saved.at] ?? []);
  status(saved.status, saved.error);
  detail(saved.detail);
  setRunning(false);
  paintPaging();
}

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
    // Focusing a box inside a folded section would do nothing visible.
    panes.reveal('pane-search');
    search.focus();
  }
});

// ---------------------------------------------------------------- workspace

const workspace = createWorkspace($('workspace'), {
  types: $('filter-types'),
  onStatus: status,
  onRootChange: (root) => {
    const line = $('workspace-root');
    // Keep the tail — the last couple of directories are what identify a project.
    line.textContent = root && root.length > 44 ? `…${root.slice(-43)}` : (root ?? '');
    line.title = root ?? '';
    line.hidden = !root;
    $('close-folder').hidden = !root;
    $('refresh-folder').hidden = !root;
    $('filter-types').hidden = !root;
  },
  onOpenFile: async (file) => {
    // Already open: go to its tab. Re-reading would be a wasted round trip, and
    // with a single click to open the tree is clicked through far more often.
    const open = buffers.all.find((buffer) => buffer.workspacePath === file.path);
    if (open) {
      buffers.activate(open);
      return;
    }
    try {
      const { text } = await api.readWorkspaceFile(file.path);
      buffers.open({ name: file.name, text, workspacePath: file.path });
      status(`opened ${file.path}`);
    } catch (e) {
      status(e.message, true);
    }
  },
  // A data file is not something to edit — it is something to read as a table, so
  // clicking one starts registering it rather than opening it.
  onAddSource: (entry) => openSourceDialog(entry),
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

/**
 * Open `path` as the workspace. Shared by the dialogue and the start screen's
 * recent list, so both do the same three follow-ups.
 */
async function openFolder(path) {
  const opened = await workspace.open(path);
  // The folder brings its own sources with it.
  await loadSources();
  paintRecent(opened.recent);
  status(`opened ${opened.root} — ${opened.files.length} file(s)`);
  // The working directory is fixed when the shell is spawned, so an open
  // terminal has to be restarted to land in the new folder.
  terminal.restart();
  return opened;
}

folderForm.addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('folder-open');
  button.disabled = true;
  folderNote();
  try {
    await openFolder(folderForm.elements.path.value.trim());
    folderDialog.close();
  } catch (e) {
    folderNote(e.message, 'bad');
  } finally {
    button.disabled = false;
  }
});

/**
 * Re-list the folder. The listing is taken once, when the folder opens, so a file
 * created afterwards — by the terminal, by another editor, by `Save as` — was
 * simply not there when we looked.
 *
 * Polled on demand rather than watched: a filesystem watcher means a second
 * long-lived channel to the browser and a per-platform notifier, for a listing
 * that costs one directory walk. `focus` is the moment that matters, because
 * creating the file happened somewhere else.
 */
async function refreshFolder({ quiet = false } = {}) {
  if (!workspace.root) return;
  const before = workspace.count;
  const state = await workspace.refresh();
  if (quiet || !state) return;
  status(`${state.files.length} file(s)${
    state.files.length === before ? '' : ` — was ${before}`}`);
}

$('refresh-folder').addEventListener('click', () => refreshFolder());
// Coming back to the tab is exactly when the tree is most likely to be stale.
addEventListener('focus', () => refreshFolder({ quiet: true }));

$('start-open-folder').addEventListener('click', () => $('open-folder').click());
$('start-new-query').addEventListener('click', () => buffers.open());
$('start-add-source').addEventListener('click', () => $('add-source').click());

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
    // The grid paints on a canvas and inherits no CSS, so it has to be told.
    grid.setTheme();
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
  onEditSource: (source) => openSourceDialog(null, source),
  // Awaited so the database list is in place before we pick from it — otherwise
  // a slow `/databases` response resets the choice a moment later.
  onSelectDatabase: async (source, db) => {
    await selectSource(source.key);
    selectDatabase(db);
  },
  onInsert: (qualified) => editor.insert(qualified),
  // Clicking a table peeks at it — in a tab of its own, so it neither touches
  // what you were writing nor throws away the result you were looking at.
  onPreview: (source, table, qualified) => {
    if (state.busy || state.inFlight) return;
    buffers.openPreview({
      name: `${table} (preview)`,
      text: previewSql(source.dialect, qualified),
    });
    run();
  },
  onTables: (source, db, tables) => {
    if (source.key !== state.source?.key) return;
    editor.addTables(tables.map((t) => `${t.schema}.${t.name}`));
  },
  onColumns: (source, db, qualified, columns) => {
    if (source.key !== state.source?.key) return;
    editor.addColumns(qualified, columns.map((c) => c.name));
  },
  onConfirm: confirmAction,
  // The server caches a schema per source and database, so re-reading the row has
  // to invalidate that too — otherwise the tree shows the new file and
  // autocompletion does not.
  onRefreshed: (source) => {
    if (source.key === state.source?.key) {
      loadSchema({ refresh: true });
      return;
    }
    // Not the active one: drop its cached snapshot so `Ctrl+K` and the next visit
    // see the folder as it is now. Fire-and-forget — nothing is waiting for it.
    api.schema(source.key, source.database, { refresh: true }).catch(() => {});
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
  // A database named by TARGET may not be in the fetched list — if the server
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

  // What a TARGET may name: the bare id, and the qualified key when telling two
  // scopes apart is the only way to be unambiguous.
  editor.setSources([
    ...new Set(
      state.sources.flatMap((source) =>
        ambiguous.has(source.id) || source.scope === 'project'
          ? [source.key, source.id]
          : [source.id],
      ),
    ),
  ]);

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

// ------------------------------------------------------------------- target

/**
 * A `TARGET` token may be a qualified `scope:id` or a bare id, when only one
 * scope defines it. Returns the key, or `null`.
 */
function resolveSourceKey(token) {
  if (state.sources.some((source) => source.key === token)) return token;
  const matches = state.sources.filter((source) => source.id === token);
  return matches.length === 1 ? matches[0].key : null;
}

/**
 * Apply any leading `TARGET <source>[.<database>]` directives. Returns the SQL
 * left to run, or `null` when a directive named something unknown.
 */
async function applyTargets(sql) {
  const parsed = parseTarget(sql, (token) => resolveSourceKey(token) !== null);

  if (parsed.unknown) {
    const known = state.sources.map((source) => source.key).join(', ') || 'none registered';
    status(`TARGET: cannot resolve \`${parsed.unknown}\` — known: ${known}`, true);
    return null;
  }

  let retargeted = null;
  for (const target of parsed.targets) {
    const key = resolveSourceKey(target.id);
    await selectSource(key);
    if (target.database) selectDatabase(target.database);
    retargeted = `${key} · ${state.database ?? ''}`;
  }

  if (retargeted) detail(retargeted);
  return { sql: parsed.sql, retargeted };
}

// -------------------------------------------------------------------- query

async function run() {
  // `state.running` is no longer the guard: a finished result keeps its socket
  // open so the next page can be read off the same query, so it stays set for as
  // long as the result is on screen. What must not overlap is a page in flight.
  if (state.busy || state.inFlight) return;
  state.busy = true;
  try {
    await execute();
  } finally {
    state.busy = false;
  }
}

async function execute() {
  const applied = await applyTargets(editor.sql().trim());
  if (!applied) return;

  const { retargeted } = applied;
  const qualified = await applyQualified(applied.sql);
  if (!qualified) return;

  const sql = qualified.sql;
  if (!sql) {
    if (retargeted) status(`now targeting ${retargeted}`);
    else status('nothing to run', true);
    return;
  }
  stream(sql);
}

/**
 * `source-name` and `source_name` are the same word to a person and different
 * ones to a lookup, and so are `Sales` and `sales`.
 */
const normalise = (name) => name.toLowerCase().replaceAll('-', '_');

/**
 * A sentence to add to a failed query when it looks like it meant to name a
 * source and got the name slightly wrong.
 *
 * Nothing can be said *before* running it — `alkyon_demo.sales.customer` is both
 * a plausible typo and ordinary three-part T-SQL. After the engine has refused,
 * a near miss is worth pointing at.
 */
function nearMissHint(sql) {
  const heads = unresolvedHeads(sql, (id) => resolveSourceKey(id) !== null);
  const matches = heads.flatMap((head) => {
    const near = state.sources.find((source) => normalise(source.id) === normalise(head));
    return near ? [`\`${head}\` → \`${near.id}\``] : [];
  });
  return matches.length
    ? `\n\nNo source is called ${matches.join(', ')}. Did you mean it? A source id with a ` +
        'dash needs quoting in SQL: `"my-source".…`'
    : '';
}

/**
 * Apply a `source.database.table` name, if the statement carries one.
 *
 * The second way to change target, and it does not replace `TARGET`: the
 * directive says where the *editor* points and stays pointed; this says where
 * one statement goes. Returns the SQL to run with the prefix removed, or `null` when the
 * statement cannot be run as written.
 */
async function applyQualified(sql) {
  // In DuckDB mode each `@attach` or `@import` names its own source and there is
  // no single target to retarget to. Retargeting there would also rewrite names
  // the federated query means literally.
  if (looksFederated(sql)) return { sql };

  const found = findCrossSource(sql, (id) => {
    const key = resolveSourceKey(id);
    if (!key) return null;
    const source = state.sources.find((s) => s.key === key);
    // A folder or file source has one catalogue and no databases, so nothing
    // between it and the name it is holding.
    return { database: source?.kind !== 'folder' && source?.kind !== 'file' };
  });
  if (!found) return { sql };

  if (found.conflict) {
    status(
      `this statement names ${found.conflict.join(' and ')} — one query goes to one ` +
        'source. To join across them, put `-- @duckdb` at the top, then `@attach` or ' +
        '`@import` each.',
      true,
    );
    return null;
  }

  const key = resolveSourceKey(found.source);
  await selectSource(key);
  if (found.database) selectDatabase(found.database);
  detail(`${key} · ${found.database ?? state.database ?? ''}`);
  return { sql: found.sql };
}

/**
 * Rows per page, from the header picker.
 *
 * *No limit* is one page big enough to hold anything, not a special case: the
 * paging machinery then simply never reaches a second page, and nothing on the
 * server allocates against the number. It is the escape hatch for the times you
 * want the whole answer — and the reason it is not the default is measured, not
 * cautious: 39.6M rows is 6.16 GB of JSON and the tab dies around 14M.
 */
function pageSize() {
  const chosen = $('page-size').value;
  return chosen === 'all' ? Number.MAX_SAFE_INTEGER : Number(chosen);
}

/**
 * How many rows of already-seen pages to keep so **‹ Prev** can go back.
 *
 * There is no page count to jump to — for a CSV or a streamed query nobody knows
 * how many pages there are until the last one arrives, which is why there is no
 * page picker. So going back means remembering, and remembering has to be
 * bounded or it becomes the very accumulation that paging exists to prevent.
 * Past this, the oldest pages are dropped and Prev stops at what is left.
 */
const HISTORY_ROWS = 250_000;

/**
 * The pages this result has produced, and where we are among them.
 *
 * `more` is what the server said about the page at the end of `pages` — going
 * back and forward again reads from here rather than from the socket, so it is
 * instant and cannot disturb the cursor.
 */
const paging = { pages: [], at: -1, more: false, first: 1 };

function resetPaging() {
  paging.pages = [];
  paging.at = -1;
  paging.more = false;
  paging.first = 1;
}

/** Remember a page, dropping the oldest ones once the budget is spent. */
function keepPage(rows) {
  paging.pages.push(rows);
  paging.at = paging.pages.length - 1;

  let held = paging.pages.reduce((total, page) => total + page.length, 0);
  while (paging.pages.length > 1 && held > HISTORY_ROWS) {
    held -= paging.pages.shift().length;
    paging.at -= 1;
    paging.first += 1;
  }
}

/** The page number on screen, counting from one. */
function pageNumber() {
  return paging.first + paging.at;
}

function paintPaging() {
  const known = paging.pages.length;
  // A result that fits in one page says nothing about pages at all — a lone
  // disabled pair of arrows is just furniture.
  const paged = paging.more || known > 1;
  const label = $('page-label');
  const previous = $('prev-page');
  const next = $('next-page');

  label.textContent = paged ? `page ${pageNumber()}` : '';
  label.hidden = !paged;
  previous.hidden = !paged;
  next.hidden = !paged;

  previous.disabled = paging.at <= 0 || state.inFlight;
  previous.title =
    paging.at > 0
      ? 'Back to the previous page'
      : paging.first > 1
        ? 'Earlier pages were dropped to keep memory bounded — run the query again to start over'
        : 'This is the first page';

  // Forward is free while there is a remembered page ahead; past that it costs
  // a read off the still-open query.
  const ahead = paging.at < known - 1;
  next.disabled = (!ahead && !paging.more) || state.inFlight || state.running === null;
  next.title = ahead
    ? 'Forward to the next page'
    : paging.more
      ? 'Read the next page of this result'
      : 'This is the last page';
}

/** Put a remembered page back on screen. */
function showPage(at) {
  paging.at = at;
  grid.replace(paging.pages[at]);
  status(`page ${pageNumber()}: ${paging.pages[at].length.toLocaleString()} row(s)`);
  paintPaging();
}

/**
 * Put the transport controls into their running state.
 *
 * `aria-busy` rather than a class of our own: it is what the CSS keys off *and*
 * what a screen reader reads, so the spinner and the announcement cannot drift
 * apart.
 */
function setRunning(running) {
  state.inFlight = running;
  const run = $('run');
  run.disabled = running;
  run.setAttribute('aria-busy', String(running));
  run.title = running
    ? 'Running — press Stop to give up on it'
    : 'Run — Ctrl+Enter, and only the selection if there is one';
  $('cancel').disabled = !running;
}

/** Send one statement to the active source and render its first page. */
function stream(sql) {
  if (!state.source) return status('no source selected', true);

  // A previous result is holding a connection open until it is let go.
  state.running?.close();
  state.running = null;
  paintPaging();

  let rows = 0;
  let sawColumns = false;
  let seen = 0;
  state.columns = null;
  resetPaging();
  setRunning(true);
  status('running…');
  detail(`${state.source.key} · ${state.database ?? state.source.database}`);

  // Closing the previous socket fires its `closed` handler a tick later, by
  // which time this query owns `state.running`. Every handler therefore checks
  // that it is still the current result before touching anything — otherwise a
  // superseded socket tears down its successor on the way out.
  let handle;
  const current = () => state.running === handle;

  handle = runQuery(
    {
      sourceId: state.source.key,
      database: state.database,
      sql,
      pageSize: pageSize(),
    },
    {
      columns: ({ columns }) => {
        sawColumns = true;
        state.columns = columns;
        grid.reset(columns);
      },
      rows: (message) => {
        rows += message.rows.length;
        grid.append(message.rows);
        status(`${(seen + rows).toLocaleString()} rows…`);
      },
      affected: ({ rows_affected }) => {
        if (!sawColumns) grid.message(`${rows_affected} row(s) affected`);
        status(`${rows_affected} row(s) affected`);
      },
      end: ({ rows: inPage, page, elapsed_ms, more }) => {
        if (!current()) return;
        if (!sawColumns && inPage === 0 && page === 1) {
          grid.message('statement completed, no rows returned');
        } else {
          grid.finish();
        }
        seen += inPage;
        rows = 0;
        keepPage(grid.take());
        paging.more = more;
        setRunning(false);
        // The count is of the whole result so far, not of this page — that is
        // the number you are actually keeping track of.
        status(
          more
            ? `page ${page}: ${inPage.toLocaleString()} row(s) in ${elapsed_ms} ms — ${seen.toLocaleString()} so far, more to come`
            : page > 1
              ? `page ${page}: ${inPage.toLocaleString()} row(s) in ${elapsed_ms} ms — ${seen.toLocaleString()} in all, the last page`
              : `${inPage.toLocaleString()} row(s) in ${elapsed_ms} ms`,
        );
        paintPaging();
      },
      cancelled: () => {
        if (!current()) return;
        status('cancelled');
        paging.more = false;
        paintPaging();
      },
      error: ({ message }) => {
        if (!current()) return;
        const full = message + nearMissHint(sql);
        grid.message(full);
        status(full, true);
      },
      closed: () => {
        if (!current()) return;
        state.running = null;
        setRunning(false);
        paintPaging();
      },
    },
  );
  state.running = handle;
}

/** Forward: to a page already held if there is one, else read the next. */
function nextPage() {
  if (state.inFlight) return;
  if (paging.at < paging.pages.length - 1) return showPage(paging.at + 1);
  if (!state.running || !paging.more) return;

  setRunning(true);
  status(`reading page ${pageNumber() + 1}…`);
  paintPaging();
  // The page just finished belongs to `paging.pages` now; the grid needs its own
  // array to fill, or it would append into the one being kept.
  grid.newPage();
  state.running.more();
}

function previousPage() {
  if (state.inFlight || paging.at <= 0) return;
  showPage(paging.at - 1);
}

$('next-page').addEventListener('click', nextPage);
$('prev-page').addEventListener('click', previousPage);

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

    // Measured from the container's *content* box, not its border box: the
    // panels sit inside `main`'s padding, so the pointer's distance from the
    // outer edge overstates the pane's width by exactly that padding.
    const box = container.getBoundingClientRect();
    const style = getComputedStyle(container);
    const before = parseFloat(axis === 'x' ? style.paddingLeft : style.paddingTop) || 0;
    const after = parseFloat(axis === 'x' ? style.paddingRight : style.paddingBottom) || 0;
    const origin = (axis === 'x' ? box.left : box.top) + before;
    const extent = (axis === 'x' ? box.width : box.height) - before - after;

    const move = (move_) => {
      const size = (axis === 'x' ? move_.clientX : move_.clientY) - origin;
      const limit = extent * max;
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

// ---------------------------------------------------------------- confirming

/**
 * Ask before something that cannot be undone. Resolves true when confirmed.
 *
 * Its own dialogue rather than the browser's `confirm`: Chrome offers to silence
 * those for the rest of the session, and a silenced confirmation is a source
 * deleted without being asked.
 */
function confirmAction({ title, detail = '', action = 'Remove' }) {
  const box = $('confirm-dialog');
  const ok = $('confirm-ok');
  const cancel = $('confirm-cancel');
  $('confirm-title').textContent = title;
  $('confirm-detail').textContent = detail;
  $('confirm-detail').hidden = !detail;
  ok.textContent = action;

  return new Promise((resolve) => {
    // The buttons answer, rather than the dialogue's `close` event: that event is
    // not delivered in every engine this runs in, and a confirmation that never
    // resolves is a button that does nothing.
    const finish = (answer) => {
      ok.removeEventListener('click', yes);
      cancel.removeEventListener('click', no);
      box.removeEventListener('cancel', no);
      if (box.open) box.close();
      resolve(answer);
    };
    const yes = () => finish(true);
    // Escape raises `cancel`, and closing without answering means no.
    const no = () => finish(false);

    ok.addEventListener('click', yes);
    cancel.addEventListener('click', no);
    box.addEventListener('cancel', no);
    box.showModal();
  });
}

// ------------------------------------------------------------ source dialog

const dialog = $('source-dialog');
const form = $('source-form');

/**
 * The authentication methods each engine can actually use, in the order the
 * dialogue should offer them.
 *
 * The backend is the authority — the PostgreSQL and MySQL connectors accept a
 * password and refuse everything else — and this keeps the dialogue from
 * offering a method whose only outcome is an error.
 */
const METHODS = {
  postgres: ['password'],
  my_sql: ['password'],
  mongo: ['password'],
  ms_sql: ['password', 'integrated', 'entra', 'aad_token'],
  // No `integrated`: the extension's Kerberos/SSPI path is not one alkyon
  // drives, and a cloud endpoint has no domain to be integrated with anyway.
  fabric: ['entra', 'aad_token', 'password'],
};

/** What each engine connects to when the Database field is left empty. */
const DEFAULT_DATABASE = {
  ms_sql: 'master',
  fabric: 'master',
  mongo: 'admin',
  my_sql: 'information_schema',
  postgres: 'postgres',
};

/** Show only the fields the selected engine and auth method actually use. */
function syncDialogFields() {
  const kind = form.elements.kind.value;
  // Azure storage sits between the two: remote, and signed in to, but read as
  // files — so it takes the format options and none of the server ones.
  const adls = kind === 'adls';
  const method = adls ? 'entra' : form.elements.method.value;
  // A folder or file is reached through the filesystem: no host, no port, no
  // encryption and nothing to authenticate.
  const server = kind !== 'folder' && kind !== 'file' && !adls;

  for (const element of form.querySelectorAll('.server-only')) element.hidden = !server;
  for (const element of form.querySelectorAll('.files-only')) element.hidden = server;
  for (const element of form.querySelectorAll('.folder-only')) element.hidden = kind !== 'folder';
  for (const element of form.querySelectorAll('.file-only')) element.hidden = kind !== 'file';
  for (const element of form.querySelectorAll('.adls-only')) element.hidden = !adls;
  for (const label of form.querySelectorAll('.mssql-only')) {
    label.hidden = kind !== 'ms_sql';
  }

  // Excel is read by calamine, in the server's own process and from a local
  // file — so it is the one format an Azure source cannot offer, since nothing is
  // copied here.
  const excel = form.querySelector('option[value="excel"]');
  excel.disabled = adls;
  excel.textContent = adls ? 'Excel — not over Azure storage' : 'Excel';
  if (adls && form.elements.format.value === 'excel') form.elements.format.value = '';

  // Only the chosen type's options. A delimiter means nothing to a parquet file,
  // and parquet and JSON have nothing to ask about at all — so until a type is
  // chosen, and for the types with no options, there is nothing to show.
  const format = server ? '' : form.elements.format.value;
  for (const element of form.querySelectorAll('.csv-only')) element.hidden = format !== 'csv';
  for (const element of form.querySelectorAll('.excel-only')) element.hidden = format !== 'excel';
  // A Delta table is a directory, so there is no list of files to leave out and
  // nothing to say about delimiters: the log describes the table.
  for (const element of form.querySelectorAll('.folder-only.files')) {
    element.hidden = element.hidden || format === 'delta';
  }
  // Say what leaving it empty will actually connect to.
  $('database-field').placeholder = `optional — defaults to ${DEFAULT_DATABASE[kind] ?? 'the engine default'}`;
  // Azure storage has one way in, so it shows the sign-in without the choice of
  // method above it.
  for (const label of form.querySelectorAll('[class^="auth-"]')) {
    label.hidden = !(server || adls) || !label.classList.contains(`auth-${method}`);
  }
  // Only the methods this engine can actually use, and **hidden** rather than
  // greyed: a disabled option still fills the list, and a list of four where
  // three can only fail reads as a choice nobody has made yet.
  //
  // PostgreSQL and MySQL take a password and nothing else. Windows integrated
  // authentication is a SQL Server concept, and so — as far as anything alkyon
  // connects to is concerned — is an Entra token.
  const usable = METHODS[kind] ?? ['password'];
  for (const option of form.elements.method.options) {
    const allowed = usable.includes(option.value);
    option.hidden = !allowed;
    option.disabled = !allowed;
  }
  if (!usable.includes(form.elements.method.value)) {
    form.elements.method.value = usable[0];
    syncDialogFields();
  }
}

/**
 * Fill the Files list from the folder itself.
 *
 * A folder source reads everything of its type, minus what is ticked here, and the
 * only place that choice can be made honestly is against what the folder actually
 * holds — typing a relative path from memory is how you exclude nothing at all.
 */
async function refreshFileList() {
  const list = $('folder-files');
  const empty = $('folder-files-empty');
  // Ticks survive a re-listing: changing the type should not silently re-include
  // a file that is still there.
  const excluded = new Set(readExclusions());
  for (const label of [...list.querySelectorAll('label')]) label.remove();

  const path = $('folder-path').value.trim();
  const format = form.elements.format.value;
  if (form.elements.kind.value !== 'folder' || !path || !format) {
    empty.textContent = 'Give a path and a type to list what is in the folder.';
    empty.hidden = false;
    return;
  }

  let files;
  try {
    ({ files } = await api.dataFiles(path, format));
  } catch (e) {
    // A path that is not there yet is the ordinary state while one is being
    // typed, so this is not an error in the note — the Test button reports those.
    empty.textContent = e.message;
    empty.hidden = false;
    return;
  }

  empty.hidden = files.length > 0;
  empty.textContent = `No ${format} file under this path.`;
  for (const file of files) {
    const label = document.createElement('label');
    label.className = 'file';
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.name = 'exclude';
    box.value = file;
    box.checked = excluded.has(file);
    const text = document.createElement('span');
    text.textContent = file;
    label.append(box, text);
    list.append(label);
  }
}

/** The files ticked for exclusion. */
function readExclusions() {
  return [...form.querySelectorAll('input[name="exclude"]:checked')].map((box) => box.value);
}

form.elements.kind.addEventListener('change', () => {
  syncDialogFields();
  refreshFileList();
});
form.elements.format.addEventListener('change', () => {
  syncDialogFields();
  refreshFileList();
});
$('folder-path').addEventListener('change', refreshFileList);
form.elements.method.addEventListener('change', syncDialogFields);

// ------------------------------------------------------------ Entra sign-in

/**
 * The sign-in the dialogue is currently holding, if any.
 *
 * A ticket rather than the tokens themselves: the refresh token goes from the
 * server's own memory to the keychain, and the page only ever learns who signed
 * in. Reset with the form, so a dialogue reopened for another source cannot
 * quietly register the previous one's account.
 */
const signIn = { ticket: null, account: null, polling: null };

function resetSignIn() {
  clearTimeout(signIn.polling);
  signIn.ticket = null;
  signIn.account = null;
  signIn.polling = null;
  paintSignIn();
}

function paintSignIn(message, kind) {
  const line = $('entra-account');
  const text = message ?? (signIn.account ? `Signed in as ${signIn.account}` : '');
  line.textContent = text;
  line.className = kind ? `note ${kind}` : 'note';
  line.hidden = !text;
}

/**
 * Watch a sign-in through to its end.
 *
 * Polled rather than pushed: it finishes in a browser this page has no channel
 * to, the wait is a couple of seconds of human time either way, and a WebSocket
 * for one event nobody is streaming would be machinery for its own sake.
 */
function watchSignIn(ticket) {
  clearTimeout(signIn.polling);
  signIn.ticket = ticket;

  const tick = async () => {
    let state;
    try {
      state = await api.signInStatus(ticket);
    } catch (e) {
      paintSignIn(e.message, 'bad');
      return;
    }
    // Still going: keep whatever the device flow gave us on screen.
    if (state.status === 'pending') {
      if (state.user_code) {
        paintSignIn(`Enter ${state.user_code} at ${state.verification_uri}`);
      }
      signIn.polling = setTimeout(tick, 1500);
      return;
    }
    if (state.status === 'ready') {
      signIn.account = state.account || 'your account';
      paintSignIn(undefined, 'good');
      return;
    }
    signIn.ticket = null;
    paintSignIn(
      state.status === 'failed' ? state.error : 'the sign-in went away — try again',
      'bad',
    );
  };

  paintSignIn('Waiting for the sign-in…');
  signIn.polling = setTimeout(tick, 600);
}

async function beginSignIn(deviceCode) {
  const data = new FormData(form);
  resetSignIn();
  try {
    const started = await api.startSignIn({
      kind: data.get('kind'),
      tenant: data.get('tenant').trim() || undefined,
      client_id: data.get('client_id').trim() || undefined,
      device_code: deviceCode,
    });
    if (started.user_code) {
      paintSignIn(`Enter ${started.user_code} at ${started.verification_uri}`);
    }
    watchSignIn(started.ticket);
  } catch (e) {
    paintSignIn(e.message, 'bad');
  }
}

$('entra-signin').addEventListener('click', () => beginSignIn(false));
$('entra-device').addEventListener('click', () => beginSignIn(true));

/**
 * The Entra half of a source's credential.
 *
 * The ticket, not the tokens: what the sign-in produced never reaches the page.
 * Empty fields are left out so the server applies its own defaults rather than
 * being told to use an empty tenant.
 */
function entraAuth(data) {
  const auth = { method: 'entra' };
  const tenant = data.get('tenant').trim();
  const clientId = data.get('client_id').trim();
  if (tenant) auth.tenant = tenant;
  if (clientId) auth.client_id = clientId;
  // Only once it has finished: an unfinished ticket cannot be redeemed, and
  // sending it would turn "still waiting" into a failure.
  if (signIn.ticket && signIn.account) auth.ticket = signIn.ticket;
  return auth;
}

/**
 * An id the form will accept, from a file or folder name: `2024 sales.csv` is
 * `2024-sales`. Empty when nothing usable is left, and then the field stays blank
 * rather than being filled with something meaningless.
 */
function suggestId(name) {
  return name
    .replace(/\.[^.]+$/, '')
    .replace(/[^A-Za-z0-9._-]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

/** The last segment of a path, whichever separator it uses. */
function basename(path) {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? '';
}

/**
 * A path in the open folder, made absolute — the only kind the dialogue can act
 * on, since its file list and *Test* both resolve what is typed against the
 * server's own working directory rather than against the open folder.
 */
function inWorkspace(relative) {
  const root = workspace.root ?? '';
  if (!relative || relative === '.') return root;
  const separator = root.includes('\\') ? '\\' : '/';
  return `${root}${separator}${relative.split('/').join(separator)}`;
}

/**
 * Open the source dialogue, empty or already pointed at something.
 *
 * `entry` comes from the file tree: a data file, or a folder holding some. What it
 * cannot say is which CSV options the files need, so those stay at their sniffed
 * defaults — the prefill saves the typing, not the thinking.
 */
/**
 * The source being edited, or `null` when the dialogue is adding one.
 *
 * Held outside the form because the *key* is what identifies the source to the
 * API, and the form's id field is precisely the thing an edit may change.
 */
let editing = null;

/** Fill the form from a summary. The credential is not in one, and must not be. */
function fillFrom(source) {
  const set = (name, value) => {
    if (form.elements[name] && value != null) form.elements[name].value = value;
  };
  set('id', source.id);
  set('scope', source.scope);
  set('kind', source.kind);
  set('tls', source.tls);
  set('method', source.auth_method);

  if (source.path) {
    if (source.kind === 'adls') {
      set('account', source.host);
      set('adls-path', source.path);
    } else if (source.kind === 'file') {
      set('file-path', source.path);
    } else {
      $('folder-path').value = source.path;
    }
    if (source.options?.format) set('format', source.options.format);
    // The rest of the format options are per-type and live under `options`.
    for (const [key, value] of Object.entries(source.options ?? {})) {
      if (key !== 'format') set(key, value);
    }
  } else {
    set('host', source.host);
    // A default port is the backend's to choose, so it is not written into the
    // box — leaving it empty keeps that true after an edit.
    if (source.port && source.port !== 0) set('port', source.port);
    set('database', source.database);
    set('instance', source.instance);
  }

  set('username', source.username);
  set('tenant', source.tenant);
  set('client_id', source.client_id);
  // Shown, not held: there is no ticket behind it, and the credential stays where
  // it is unless a fresh sign-in replaces it.
  if (source.account) paintSignIn(`Signed in as ${source.account}`);
}

/**
 * Whether this submission means "keep the credential already in the vault".
 *
 * The password never comes back out of the API, so a reopened form cannot send it
 * back in. An untouched box is therefore the only way to say *unchanged* — and a
 * changed sign-in method is the one case where the stored credential is no use,
 * since a refresh token is not a password.
 */
function keepingSecret() {
  if (!editing) return false;
  const method = form.elements.kind.value === 'adls' ? 'entra' : form.elements.method.value;
  if (method !== editing.auth_method) return false;
  switch (method) {
    case 'password':
      return !form.elements.password.value;
    case 'aad_token':
      return !form.elements.token.value;
    // A finished sign-in in the dialogue replaces the stored one; without it,
    // the refresh token already in the vault is what keeps the source working.
    case 'entra':
      return !(signIn.ticket && signIn.account);
    default:
      return true;
  }
}

function openSourceDialog(entry = null, source = null) {
  editing = source;
  form.reset();
  note();
  $('source-title').textContent = source ? `Edit ${source.key}` : 'Register a source';
  $('source-save').textContent = source ? 'Connect and save' : 'Connect and save';
  $('password-hint').hidden = !source;
  // `form.reset()` does not know about a sign-in held outside the form.
  resetSignIn();
  // A project source has nowhere to live without an open folder.
  const project = form.querySelector('option[value="project"]');
  project.disabled = !workspace.root;
  project.textContent = workspace.root
    ? 'This folder — .alkyon/sources.json, committable'
    : 'This folder — needs an open folder';

  if (entry) {
    form.elements.kind.value = entry.directory ? 'folder' : 'file';
    if (entry.format) form.elements.format.value = entry.format;
    const path = inWorkspace(entry.path);
    if (entry.directory) $('folder-path').value = path;
    else form.elements['file-path'].value = path;
    // The folder's own name, and for the root row the open folder's.
    const named = entry.name ?? entry.path.split('/').pop();
    form.elements.id.value = suggestId(named || basename(workspace.root ?? ''));
    // It came out of the open folder, so that is where it belongs.
    form.elements.scope.value = 'project';
  }

  if (source) fillFrom(source);

  syncDialogFields();
  // `form.reset()` restores the values, but the last folder's file list is still
  // in the DOM.
  for (const label of [...$('folder-files').querySelectorAll('label')]) label.remove();
  refreshFileList();
  dialog.showModal();
}

$('add-source').addEventListener('click', () => openSourceDialog());

$('source-cancel').addEventListener('click', () => {
  editing = null;
  dialog.close();
});

/** The one line of feedback under the form. No argument clears it. */
function note(text, kind) {
  const element = $('source-note');
  element.textContent = text ?? '';
  element.className = kind ? `note ${kind}` : 'note';
  element.hidden = !text;
}

/**
 * The format half of a folder or file source.
 *
 * Every empty box is left out entirely rather than sent as `""`: absent means
 * "let DuckDB work it out", and its sniffer is better at that than a blank string
 * would be at anything.
 */
function readFileOptions(data) {
  const options = {};
  const format = data.get('format');
  if (format) options.format = format;

  // Everything of that type is read unless it was ticked to be left out. Only
  // ticked boxes reach the form data, so this is exactly the exclusion list.
  const excluded = data.getAll('exclude');
  if (excluded.length) options.exclude = excluded;

  const csv = {};
  const text = (field, name) => {
    const value = (data.get(field) ?? '').trim();
    if (value) csv[name] = value;
  };
  text('csv-delimiter', 'delimiter');
  text('csv-quote', 'quote');
  text('csv-escape', 'escape');
  text('csv-decimal', 'decimal');
  text('csv-encoding', 'encoding');
  text('csv-null', 'null_string');
  text('csv-date', 'date_format');
  text('csv-timestamp', 'timestamp_format');

  const header = data.get('csv-header');
  if (header) csv.header = header === 'true';
  if (data.get('csv-all-varchar')) csv.all_varchar = true;
  if (data.get('csv-ignore-errors')) csv.ignore_errors = true;

  const skip = data.get('csv-skip');
  if (skip) csv.skip = Number(skip);
  const sample = data.get('csv-sample');
  if (sample) csv.sample_size = Number(sample);

  if (Object.keys(csv).length) options.csv = csv;

  const sheet = (data.get('excel-sheet') ?? '').trim();
  if (sheet) options.excel = { sheet };

  return options;
}

/** Read the form into the `POST /sources` shape. */
function readForm() {
  const data = new FormData(form);
  const kind = data.get('kind');

  // A folder or file source sends a path and its format options, and nothing
  // else; the fields the form still holds for the other engines would only be
  // noise on the wire.
  if (kind === 'folder' || kind === 'file') {
    return {
      id: data.get('id').trim(),
      scope: data.get('scope'),
      kind,
      path: (kind === 'file' ? data.get('file-path') : data.get('path')).trim(),
      options: readFileOptions(data),
      auth: { method: 'none' },
    };
  }

  // Remote like a server, read like a folder: it carries both a host and the
  // format options, and its only way in is a sign-in.
  if (kind === 'adls') {
    return {
      id: data.get('id').trim(),
      scope: data.get('scope'),
      kind,
      host: data.get('account').trim(),
      path: data.get('adls-path').trim(),
      options: readFileOptions(data),
      auth: entraAuth(data),
    };
  }

  const method = data.get('method');
  const auth = method === 'entra' ? entraAuth(data) : { method };
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

/**
 * What the form cannot say with `required`.
 *
 * A field the engine does not use is hidden, and a hidden required field makes the
 * form unsubmittable outright — so the fields that matter are checked here.
 * Returns the complaint, or nothing when there is none.
 */
function complaint(config) {
  // An edit that keeps the stored credential has nothing to sign in for: the
  // refresh token in the vault is what the source has been using all along.
  if (config.auth.method === 'entra' && !config.auth.ticket && !keepingSecret()) {
    return signIn.ticket ? 'wait for the sign-in to finish' : 'sign in first';
  }
  if (config.kind === 'adls') {
    if (!config.host) return 'give an account or hostname';
    if (!config.options.format) return 'choose the file type first';
    return config.path ? null : 'give a container and folder';
  }
  if (config.kind !== 'folder' && config.kind !== 'file') {
    return config.host ? null : 'give a host';
  }
  if (!config.options.format) return 'choose the file type first';
  return config.path ? null : 'give a path';
}

$('source-test').addEventListener('click', () => {
  const config = readForm();
  const files = config.kind === 'folder' || config.kind === 'file';
  const wrong = complaint(config);
  if (wrong) return note(`${wrong} to test`, 'bad');

  withFeedback($('source-test'), 'Testing…', async () => {
    // Testing an edit has to use the same credential saving it would, or the
    // button would contradict the sentence under the password box.
    const result = await api.testConnection(
      keepingSecret() ? { ...config, keep_secret_of: editing.key } : config,
    );
    note(
      files
        ? `Readable in ${result.latency_ms} ms.`
        : `Connected to ${result.database} in ${result.latency_ms} ms.`,
      'good',
    );
  });
});

form.addEventListener('submit', (event) => {
  event.preventDefault();
  const config = readForm();
  const wrong = complaint(config);
  if (wrong) return note(wrong, 'bad');
  withFeedback($('source-save'), 'Connecting…', async () => {
    const summary = editing
      ? await api.updateSource(editing.key, { ...config, keep_secret: keepingSecret() })
      : await api.addSource(config);
    const renamed = editing && editing.key !== summary.key;
    dialog.close();
    // The ticket was spent registering it; holding on to a stale one would let
    // the next dialogue think it is already signed in.
    resetSignIn();
    await loadSources({ keep: false });
    await selectSource(summary.key);
    status(
      editing
        ? renamed
          ? `renamed ${editing.key} to ${summary.key}`
          : `updated ${summary.key}`
        : `registered ${summary.key}`,
    );
    editing = null;
  });
});

// ------------------------------------------------------------------ startup

theme.apply();

// The first tab adopts whatever the editor was seeded with, so the hint text in
// it survives.
// No tab is opened here on purpose: alkyon comes up on the start screen, where
// the first thing to decide is where you are working rather than what to type.
// `Alt+N`, the `+` in the tab bar, opening a `.sql` file or clicking a table all
// open one.
paintStart();
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

const [, , opened] = await Promise.all([loadSources(), loadShells(), workspace.refresh()]);
paintRecent(opened?.recent);
