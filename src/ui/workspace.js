// The open folder: a flat-but-grouped tree of the files under it alkyon can do
// something with — `.sql` to edit, data files to register as a source.
//
// The root is a path on the machine running alkyon, not one picked in the
// browser — `showDirectoryPicker()` never reveals a path, and the terminal has to
// be able to start there.

import { api } from './api.js';

/**
 * Which extensions the tree leaves out, remembered across runs.
 *
 * The *hidden* ones rather than the shown ones, so that a type this folder does
 * not hold today — or one a later alkyon learns to read — arrives visible instead
 * of silently missing from a list written before it existed.
 */
const HIDDEN_KEY = 'alkyon.hidden-types';

function loadHidden() {
  try {
    const stored = JSON.parse(localStorage.getItem(HIDDEN_KEY));
    return new Set(Array.isArray(stored) ? stored : []);
  } catch {
    return new Set();
  }
}

/** `customers.csv` is `csv`. The server lists nothing without an extension. */
function extensionOf(file) {
  return file.name.split('.').pop().toLowerCase();
}

/**
 * What the filter button says: `all`, the types themselves while there are few
 * enough to read, then how many.
 *
 * `all` rather than listing every type even when they all show: the question the
 * button answers is "am I looking at everything?", and a list of five extensions
 * makes that a thing to work out rather than a thing to read.
 *
 * Measured against what this folder holds, not against the whole hidden set — a
 * type hidden while another folder was open is not this folder's business, and
 * counting it here would say `4 types` over a tree showing five.
 */
export function typeLabel(extensions, hidden) {
  const shown = extensions.filter((extension) => !hidden.has(extension));
  if (shown.length === extensions.length) return 'all';
  if (shown.length === 0) return 'none';
  if (shown.length <= 2) return shown.join(' ');
  return `${shown.length} types`;
}

export function createWorkspace(
  element,
  { types, onOpenFile, onAddSource, onRootChange, onStatus },
) {
  let root = null;
  /**
   * How many files the folder holds — filter or no filter, since a refresh
   * reports what the folder gained, not what the filter lets through.
   */
  let count = 0;
  /** The whole listing. The filter is a view of it, not a re-read. */
  let listing = [];
  let truncated = false;
  let hidden = loadHidden();
  /** The type menu, while it is open. */
  let menu = null;

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

  function select(row) {
    for (const selected of element.querySelectorAll('.row.selected')) {
      selected.classList.remove('selected');
    }
    row.classList.add('selected');
  }

  /**
   * The one format a folder could be read as, or `null` when it holds none or
   * several. A source reads one format, so guessing between two would be picking
   * for the user; guessing when there is only one candidate is not a guess.
   */
  function soleFormat(files) {
    const formats = new Set(files.map((file) => file.format).filter(Boolean));
    return formats.size === 1 ? [...formats][0] : null;
  }

  function fileRow(file) {
    const node = document.createElement('div');
    node.className = 'node';

    const row = document.createElement('div');
    row.className = 'row';
    const size = `${file.bytes.toLocaleString()} bytes`;
    row.title = file.format
      ? `${file.path} — ${size} — register as a source`
      : `${file.path} — ${size}`;

    const twisty = document.createElement('span');
    twisty.className = 'twisty leaf';

    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = file.name;

    row.append(twisty, label);

    // Data files sit next to the `.sql` ones and do something else entirely when
    // clicked, so they say what they are.
    if (file.format) {
      const badge = document.createElement('span');
      badge.className = 'badge';
      badge.textContent = file.name.split('.').pop().toLowerCase();
      row.append(badge);
    }

    // A single click: opening a file is the thing you came to the tree for, and
    // making it the second click only hides it.
    row.addEventListener('click', () => {
      select(row);
      if (file.format) onAddSource({ path: file.path, name: file.name, format: file.format });
      else onOpenFile(file);
    });
    node.append(row);
    return node;
  }

  function folderNode(name, files) {
    const node = document.createElement('div');
    node.className = 'node';
    node.setAttribute('aria-expanded', 'true');

    const row = document.createElement('div');
    row.className = 'row';
    row.title = `${name || './'} — register as a source`;

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
    // The twisty folds; the row registers the folder. Two things one click apart,
    // which is why folding lives on the arrow rather than on the whole row.
    twisty.addEventListener('click', (event) => {
      event.stopPropagation();
      toggle();
    });
    row.addEventListener('click', () => {
      select(row);
      onAddSource({ path: name, directory: true, format: soleFormat(files) });
    });

    node.append(row, children);
    return node;
  }

  // -------------------------------------------------------------- type filter

  /** The extensions this folder actually holds, with how many of each. */
  function present() {
    const counts = new Map();
    for (const file of listing) {
      const extension = extensionOf(file);
      counts.set(extension, (counts.get(extension) ?? 0) + 1);
    }
    return [...counts.entries()].sort(([a], [b]) => a.localeCompare(b));
  }

  /** What the button says: the filter is worth seeing without opening it. */
  function paintTypes() {
    if (!types) return;
    types.textContent = typeLabel(
      present().map(([extension]) => extension),
      hidden,
    );
  }

  function setHidden(next) {
    hidden = next;
    try {
      localStorage.setItem(HIDDEN_KEY, JSON.stringify([...hidden]));
    } catch {
      // A browser refusing storage is a preference not remembered, not a failure.
    }
    paint();
  }

  /**
   * The tickable list of types, under the button that opened it.
   *
   * Built on the grid's filter popover — same shape, same dismissal — because it
   * is the same gesture: narrow what is on screen without changing what was read.
   */
  function openTypeMenu() {
    closeTypeMenu();
    const pop = document.createElement('div');
    pop.className = 'filter-pop';

    const title = document.createElement('div');
    title.className = 'filter-title';
    title.textContent = 'Show file types';

    const list = document.createElement('div');
    list.className = 'filter-list';

    const entries = present();
    /** Every box, so a tick anywhere can put the others straight. */
    const boxes = [];

    const choice = (label, extension, badge) => {
      const row = document.createElement('label');
      row.className = 'filter-choice';
      const box = document.createElement('input');
      box.type = 'checkbox';
      const text = document.createElement('span');
      text.textContent = label;
      row.append(box, text);
      if (badge !== undefined) {
        const howMany = document.createElement('span');
        howMany.className = 'badge';
        howMany.textContent = String(badge);
        row.append(howMany);
      }
      list.append(row);
      boxes.push({ extension, box });
      return box;
    };

    // Ticked in place rather than rebuilt: a rebuild would take the focus off the
    // box that was just ticked, which makes the list unusable from the keyboard.
    const all = () => entries.every(([extension]) => !hidden.has(extension));
    const sync = () => {
      for (const { extension, box } of boxes) {
        box.checked = extension === null ? all() : !hidden.has(extension);
      }
    };

    // All or nothing, in one tick either way. Hiding every type leaves an empty
    // tree that says so — which beats an "All" that cannot be unticked.
    choice('All', null).addEventListener('change', (event) => {
      // Only this folder's types either way: showing all of them must not undo a
      // type hidden somewhere else, and hiding all of them means these.
      const next = new Set(hidden);
      for (const [extension] of entries) {
        if (event.target.checked) next.delete(extension);
        else next.add(extension);
      }
      setHidden(next);
      sync();
    });

    for (const [extension, howMany] of entries) {
      choice(extension, extension, howMany).addEventListener('change', (event) => {
        const next = new Set(hidden);
        if (event.target.checked) next.delete(extension);
        else next.add(extension);
        setHidden(next);
        sync();
      });
    }
    sync();

    pop.append(title, list);
    document.body.append(pop);

    // Under the button, pulled back inside the window like the grid's popover.
    const anchor = types.getBoundingClientRect();
    const { width } = pop.getBoundingClientRect();
    pop.style.left = `${Math.max(8, Math.min(anchor.left, window.innerWidth - width - 8))}px`;
    pop.style.top = `${anchor.bottom + 2}px`;

    // The button is not "away": without this the click that should close the menu
    // closes it here and reopens it in the button's own handler.
    const away = (event) => {
      if (!pop.contains(event.target) && !types.contains(event.target)) closeTypeMenu();
    };
    const escape = (event) => {
      if (event.key === 'Escape') closeTypeMenu();
    };
    // On the next frame: the click that opened it is still propagating.
    const frame = requestAnimationFrame(() => {
      document.addEventListener('pointerdown', away);
      document.addEventListener('keydown', escape);
    });
    menu = { pop, away, escape, frame };
  }

  function closeTypeMenu() {
    if (!menu) return;
    cancelAnimationFrame(menu.frame);
    document.removeEventListener('pointerdown', menu.away);
    document.removeEventListener('keydown', menu.escape);
    menu.pop.remove();
    menu = null;
  }

  types?.addEventListener('click', () => (menu ? closeTypeMenu() : openTypeMenu()));

  // --------------------------------------------------------------------- tree

  function paint() {
    paintTypes();
    const files = listing.filter((file) => !hidden.has(extensionOf(file)));
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
      empty.textContent = listing.length
        ? 'Every file type is hidden — pick one above.'
        : 'Nothing alkyon can open or read here.';
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

  /** Take a listing from the server and draw it. */
  function adopt(state) {
    listing = state.files;
    truncated = state.truncated;
    count = listing.length;
    closeTypeMenu();
    paint();
  }

  return {
    get root() {
      return root;
    },

    get count() {
      return count;
    },

    async refresh() {
      try {
        const state = await api.workspace();
        root = state.root;
        onRootChange(root);
        adopt(state);
        return state;
      } catch (e) {
        onStatus(e.message, true);
        return null;
      }
    },

    async open(path) {
      const state = await api.openWorkspace(path);
      root = state.root;
      onRootChange(root);
      adopt(state);
      return state;
    },

    async close() {
      await api.closeWorkspace();
      root = null;
      onRootChange(null);
      adopt({ files: [], truncated: false });
    },
  };
}
