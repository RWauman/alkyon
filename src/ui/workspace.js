// The open folder: a flat-but-grouped tree of the `.sql` files under it.
//
// The root is a path on the machine running alkyon, not one picked in the
// browser — `showDirectoryPicker()` never reveals a path, and the terminal has to
// be able to start there.

import { api } from './api.js';

export function createWorkspace(element, { onOpenFile, onRootChange, onStatus }) {
  let root = null;

  /** Group `a/b/c.sql` paths into one collapsible node per directory. */
  function group(files) {
    const folders = new Map();
    for (const file of files) {
      const cut = file.path.lastIndexOf('/');
      const folder = cut === -1 ? '' : file.path.slice(0, cut);
      if (!folders.has(folder)) folders.set(folder, []);
      folders.get(folder).push(file);
    }
    return [...folders.entries()].sort(([a], [b]) => a.localeCompare(b));
  }

  function fileRow(file) {
    const node = document.createElement('div');
    node.className = 'node';

    const row = document.createElement('div');
    row.className = 'row';
    row.title = `${file.path} — ${file.bytes.toLocaleString()} bytes`;

    const twisty = document.createElement('span');
    twisty.className = 'twisty leaf';

    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = file.name;

    row.append(twisty, label);
    row.addEventListener('click', () => {
      for (const selected of element.querySelectorAll('.row.selected')) {
        selected.classList.remove('selected');
      }
      row.classList.add('selected');
    });
    row.addEventListener('dblclick', () => onOpenFile(file));
    node.append(row);
    return node;
  }

  function folderNode(name, files) {
    const node = document.createElement('div');
    node.className = 'node';
    node.setAttribute('aria-expanded', 'true');

    const row = document.createElement('div');
    row.className = 'row';

    const twisty = document.createElement('span');
    twisty.className = 'twisty';
    twisty.textContent = '▾';

    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = name || './';

    const badge = document.createElement('span');
    badge.className = 'badge';
    badge.textContent = String(files.length);

    row.append(twisty, label, badge);

    const children = document.createElement('div');
    children.className = 'children';
    children.append(...files.map(fileRow));

    const toggle = () => {
      const open = node.getAttribute('aria-expanded') === 'true';
      node.setAttribute('aria-expanded', String(!open));
      twisty.textContent = open ? '▸' : '▾';
    };
    twisty.addEventListener('click', (event) => {
      event.stopPropagation();
      toggle();
    });
    row.addEventListener('dblclick', toggle);

    node.append(row, children);
    return node;
  }

  function paint({ files, truncated }) {
    if (!root) {
      const empty = document.createElement('div');
      empty.className = 'empty muted';
      empty.textContent = 'No folder open.';
      element.replaceChildren(empty);
      return;
    }
    if (files.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'empty muted';
      empty.textContent = 'No .sql files here.';
      element.replaceChildren(empty);
      return;
    }

    const nodes = group(files).map(([folder, group_]) => folderNode(folder, group_));
    if (truncated) {
      const note = document.createElement('div');
      note.className = 'empty muted';
      note.textContent = 'listing truncated — too many files';
      nodes.push(note);
    }
    element.replaceChildren(...nodes);
  }

  return {
    get root() {
      return root;
    },

    async refresh() {
      try {
        const state = await api.workspace();
        root = state.root;
        onRootChange(root);
        paint(state);
      } catch (e) {
        onStatus(e.message, true);
      }
    },

    async open(path) {
      const state = await api.openWorkspace(path);
      root = state.root;
      onRootChange(root);
      paint(state);
      return state;
    },

    async close() {
      await api.closeWorkspace();
      root = null;
      onRootChange(null);
      paint({ files: [], truncated: false });
    },
  };
}
