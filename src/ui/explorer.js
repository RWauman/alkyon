// The sidebar tree: source → database → schema → table → column.
//
// Every level loads on first expand, so opening the workbench costs one request
// and browsing a large server never enumerates more than you look at.

import { api } from './api.js';

/**
 * Build one tree row.
 *
 * @param {object} spec
 * @param {string} spec.label
 * @param {string} [spec.badge]        dimmed suffix — a type, a row count
 * @param {boolean} [spec.pk]          style the badge as a key
 * @param {() => Promise<Node[]>} [spec.load]  children, fetched on first expand
 * @param {() => void} [spec.onSelect]
 * @param {() => void} [spec.onActivate]  double click
 * @param {() => void} [spec.onRemove]    shows a delete affordance
 */
function makeNode({ label, badge, pk, dot, scope, load, onSelect, onActivate, onRemove }) {
  const node = document.createElement('div');
  node.className = 'node';
  node.setAttribute('role', 'treeitem');

  const row = document.createElement('div');
  row.className = 'row';

  const twisty = document.createElement('span');
  twisty.className = load ? 'twisty' : 'twisty leaf';
  twisty.textContent = '▸';

  const text = document.createElement('span');
  text.className = 'label';
  text.textContent = label;
  text.title = label;

  row.append(twisty);
  if (dot) row.append(dot);
  row.append(text);

  if (scope) {
    const chip = document.createElement('span');
    chip.className = `scope scope-${scope}`;
    chip.textContent = scope === 'project' ? 'project' : 'user';
    chip.title =
      scope === 'project'
        ? 'defined in .alkyon/sources.json in the open folder'
        : "defined in your own registry";
    row.append(chip);
  }

  if (badge) {
    const tag = document.createElement('span');
    tag.className = pk ? 'badge pk' : 'badge';
    tag.textContent = badge;
    row.append(tag);
  }

  if (onRemove) {
    const spacer = document.createElement('span');
    spacer.className = 'spacer';
    const drop = document.createElement('button');
    drop.className = 'drop';
    drop.title = `Remove ${label}`;
    drop.textContent = '✕';
    drop.addEventListener('click', (event) => {
      event.stopPropagation();
      onRemove();
    });
    row.append(spacer, drop);
  }

  const children = document.createElement('div');
  children.className = 'children';

  node.append(row, children);

  if (!load) {
    node.setAttribute('aria-expanded', 'false');
  } else {
    node.setAttribute('aria-expanded', 'false');
    let loaded = false;

    const toggle = async () => {
      const open = node.getAttribute('aria-expanded') === 'true';
      if (open) {
        node.setAttribute('aria-expanded', 'false');
        twisty.textContent = '▸';
        return;
      }
      node.setAttribute('aria-expanded', 'true');
      twisty.textContent = '▾';
      if (loaded) return;
      loaded = true;

      const pending = document.createElement('div');
      pending.className = 'empty muted';
      pending.textContent = 'loading…';
      children.replaceChildren(pending);

      try {
        const built = await load();
        if (built.length === 0) {
          const empty = document.createElement('div');
          empty.className = 'empty muted';
          empty.textContent = 'empty';
          children.replaceChildren(empty);
        } else {
          children.replaceChildren(...built);
        }
      } catch (e) {
        // Let the row be retried: a server that was down may come back.
        loaded = false;
        const failed = document.createElement('div');
        failed.className = 'failed';
        failed.textContent = e.message;
        children.replaceChildren(failed);
      }
    };

    twisty.addEventListener('click', (event) => {
      event.stopPropagation();
      toggle();
    });
    row.addEventListener('dblclick', () => {
      if (!onActivate) toggle();
    });
  }

  row.addEventListener('click', () => {
    for (const selected of node.closest('.tree').querySelectorAll('.row.selected')) {
      selected.classList.remove('selected');
    }
    row.classList.add('selected');
    onSelect?.();
  });

  if (onActivate) row.addEventListener('dblclick', onActivate);

  return node;
}

/**
 * @param {object} hooks
 * @param {(source: object) => void} hooks.onSelectSource
 * @param {(source: object, db: string) => void} hooks.onSelectDatabase
 * @param {(qualified: string) => void} hooks.onInsert   double-clicked a table
 * @param {(source, db, tables) => void} hooks.onTables  autocomplete warm-up
 * @param {(source, db, table, columns) => void} hooks.onColumns
 * @param {(message: string, isError?: boolean) => void} hooks.onStatus
 */
export function createExplorer(element, hooks) {
  // SQL Server takes brackets, PostgreSQL takes double quotes.
  const quote = (source, name) =>
    source.dialect === 'tsql'
      ? `[${name.replaceAll(']', ']]')}]`
      : `"${name.replaceAll('"', '""')}"`;

  const qualify = (source, schema, name) =>
    `${quote(source, schema)}.${quote(source, name)}`;

  function columnNode(column) {
    const parts = [column.data_type];
    if (!column.nullable) parts.push('not null');
    return makeNode({
      label: column.name,
      badge: `${column.is_primary_key ? 'PK ' : ''}${parts.join(' ')}`,
      pk: column.is_primary_key,
    });
  }

  function tableNode(source, db, table) {
    const plain = `${table.schema}.${table.name}`;
    return makeNode({
      label: table.name,
      badge: table.kind === 'view' ? 'view' : undefined,
      onSelect: () => hooks.onSelectDatabase(source, db),
      onActivate: () => hooks.onInsert(qualify(source, table.schema, table.name)),
      load: async () => {
        // Expanding is as much a statement of intent as clicking, and the schema
        // we are about to load only feeds autocomplete for the active source.
        await hooks.onSelectDatabase(source, db);
        const columns = await api.columns(source.key, db, table.schema, table.name);
        hooks.onColumns(source, db, plain, columns);
        return columns.map(columnNode);
      },
    });
  }

  function schemaNode(source, db, schema, tables) {
    return makeNode({
      label: schema,
      badge: String(tables.length),
      load: async () => tables.map((table) => tableNode(source, db, table)),
    });
  }

  function databaseNode(source, db) {
    return makeNode({
      label: db,
      onSelect: () => hooks.onSelectDatabase(source, db),
      load: async () => {
        await hooks.onSelectDatabase(source, db);
        const tables = await api.tables(source.key, db);
        hooks.onTables(source, db, tables);
        const bySchema = new Map();
        for (const table of tables) {
          if (!bySchema.has(table.schema)) bySchema.set(table.schema, []);
          bySchema.get(table.schema).push(table);
        }
        return [...bySchema.entries()]
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([schema, group]) => schemaNode(source, db, schema, group));
      },
    });
  }

  /**
   * Probe the source and colour its dot. Checked on load and on refresh only —
   * a workbench polling every registered server would be rude.
   */
  async function probe(source, dot) {
    dot.className = 'dot checking';
    dot.title = `checking ${source.key}…`;
    try {
      const result = await api.status(source.key);
      dot.className = result.ok ? 'dot ok' : 'dot down';
      dot.title = result.ok
        ? `${source.key} reachable — ${result.latency_ms} ms`
        : `${source.key} unreachable — ${result.error}`;
    } catch (e) {
      dot.className = 'dot down';
      dot.title = `${source.key} — ${e.message}`;
    }
    hooks.onProbe?.(source, dot.className.includes('ok'), dot.title);
  }

  function sourceNode(source) {
    const dot = document.createElement('span');
    dot.className = 'dot';
    probe(source, dot);

    return makeNode({
      label: source.id,
      dot,
      scope: source.scope,
      badge: `${source.dialect} · ${source.host}:${source.port}`,
      onSelect: () => hooks.onSelectSource(source),
      onRemove: async () => {
        if (!confirm(`Remove ${source.key}? Its credential is deleted from the vault.`)) return;
        try {
          await api.removeSource(source.key);
          hooks.onStatus(`removed ${source.key}`);
          await refresh();
        } catch (e) {
          hooks.onStatus(e.message, true);
        }
      },
      load: async () => {
        hooks.onSelectSource(source);
        const databases = await api.databases(source.key);
        return databases.map((db) => databaseNode(source, db));
      },
    });
  }

  async function refresh() {
    try {
      const sources = await api.sources();
      if (sources.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'empty muted';
        empty.textContent = 'No sources yet — use + to add one.';
        element.replaceChildren(empty);
      } else {
        element.replaceChildren(...sources.map(sourceNode));
      }
      return sources;
    } catch (e) {
      hooks.onStatus(e.message, true);
      return [];
    }
  }

  return { refresh };
}
