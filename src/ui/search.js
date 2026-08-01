// The search bar over every indexed schema: tables, columns, and column types.
//
// Only cached schemas are searched, so the footer says what is covered and offers
// to index the rest — under-reporting silently would be worse than being slow.

import { api } from './api.js';

const DEBOUNCE_MS = 180;

export function createSearch(
  { input, results, coverage, indexAll },
  { onPick, onStatus },
) {
  let timer = null;
  let lastQuery = '';

  function paintCoverage(indexed) {
    if (indexed.length === 0) {
      coverage.textContent = 'nothing indexed yet';
      indexAll.hidden = false;
      return;
    }
    const columns = indexed.reduce((total, entry) => total + entry.columns, 0);
    coverage.textContent = `${indexed.length} database(s), ${columns.toLocaleString()} columns`;
    coverage.title = indexed.map((e) => `${e.source} · ${e.database} (${e.columns})`).join('\n');
    indexAll.hidden = false;
  }

  function row(hit) {
    const node = document.createElement('div');
    node.className = 'hit';
    node.title = `${hit.source} · ${hit.database} · ${hit.schema}.${hit.table}${
      hit.column ? `.${hit.column}` : ''
    }`;

    const kind = document.createElement('span');
    kind.className = `hit-kind hit-${hit.kind}`;
    kind.textContent = { table: 'TBL', view: 'VW', column: 'COL' }[hit.kind] ?? '?';

    const name = document.createElement('span');
    name.className = 'hit-name';
    name.textContent = hit.column ? `${hit.table}.${hit.column}` : `${hit.schema}.${hit.table}`;

    const line = document.createElement('span');
    line.className = 'hit-line';
    line.append(name);
    if (hit.is_primary_key) {
      const pk = document.createElement('span');
      pk.className = 'badge pk';
      pk.textContent = 'PK';
      line.append(pk);
    }

    // Second line, because which source a hit came from is the whole point of
    // searching across them — and three facts do not fit on one sidebar row.
    const meta = document.createElement('span');
    meta.className = 'hit-meta';
    const where = hit.source.startsWith('user:') ? hit.source.slice(5) : hit.source;
    meta.textContent = [hit.data_type, where, hit.database].filter(Boolean).join(' · ');

    node.append(kind, line, meta);

    node.addEventListener('click', () => onPick(hit));
    return node;
  }

  async function run(query) {
    if (!query.trim()) {
      results.replaceChildren();
      return;
    }
    try {
      const { hits, indexed } = await api.search(query);
      paintCoverage(indexed);

      if (hits.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'empty muted';
        empty.textContent = indexed.length
          ? 'no match in what is indexed'
          : 'nothing indexed — use “Index all”';
        results.replaceChildren(empty);
        return;
      }
      results.replaceChildren(...hits.map(row));
    } catch (e) {
      onStatus(e.message, true);
    }
  }

  input.addEventListener('input', () => {
    const query = input.value;
    if (query === lastQuery) return;
    lastQuery = query;
    // Debounced: a keystroke per request would be pointless work.
    clearTimeout(timer);
    timer = setTimeout(() => run(query), DEBOUNCE_MS);
  });

  input.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      input.value = '';
      lastQuery = '';
      results.replaceChildren();
      input.blur();
    }
  });

  return {
    focus() {
      input.focus();
      input.select();
    },

    /** Re-run the current query — after indexing, the answer may have changed. */
    rerun() {
      return run(input.value);
    },

    async refreshCoverage() {
      try {
        const { indexed } = await api.search('');
        paintCoverage(indexed);
      } catch {
        /* the status line already says the backend is unreachable */
      }
    },
  };
}
