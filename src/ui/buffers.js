// Open buffers and the tab bar.
//
// One CodeMirror `Doc` per buffer, swapped into the single editor — that keeps
// undo history, cursor and scroll position per tab for free. Each buffer also
// remembers which source and database it was last run against, so switching tabs
// switches target the way SSMS and DataGrip do.

/**
 * `items` with the one at `from` moved so that it sits **before** `to`.
 *
 * `to` indexes the list as it is *now*, before the move — which is what a drop
 * target naturally gives you — so removing the dragged item first shifts every
 * later index down by one. Getting that wrong is an off-by-one that only shows up
 * when you drag rightwards, which is why it is a function with tests rather than
 * two lines inside an event handler.
 */
export function reorder(items, from, to) {
  const next = [...items];
  const [moving] = next.splice(from, 1);
  next.splice(to > from ? to - 1 : to, 0, moving);
  return next;
}

export function createBuffers(bar, { onActivate, onDirtyChange }) {
  /** @type {Array<{id:number,name:string,doc:any,handle:any,generation:number,target:object|null}>} */
  const buffers = [];
  let active = null;
  let sequence = 0;

  const isDirty = (buffer) => !buffer.doc.isClean(buffer.generation);

  /** The tab being dragged, while it is being dragged. */
  let dragging = null;

  /** Clear the insertion marks, wherever they ended up. */
  function unmark() {
    for (const marked of bar.querySelectorAll('.drop-before, .drop-after')) {
      marked.classList.remove('drop-before', 'drop-after');
    }
  }

  /**
   * Reordering by drag, over the native drag API rather than pointer events.
   *
   * The native one is what gives the drag image, the cursor and the escape-to-cancel
   * for free; hand-rolling it with pointer capture would mean reimplementing all
   * three. The price is `dataTransfer.setData`, which Firefox requires before it
   * will start a drag at all.
   */
  function makeDraggable(tab, buffer) {
    tab.draggable = true;

    tab.addEventListener('dragstart', (event) => {
      dragging = buffer;
      event.dataTransfer.effectAllowed = 'move';
      event.dataTransfer.setData('text/plain', buffer.name);
    });

    tab.addEventListener('dragover', (event) => {
      if (!dragging || dragging === buffer) return;
      // Nothing is dropped anywhere unless the default is prevented.
      event.preventDefault();
      event.stopPropagation();
      const box = tab.getBoundingClientRect();
      const after = event.clientX > box.left + box.width / 2;
      unmark();
      tab.classList.add(after ? 'drop-after' : 'drop-before');
    });

    tab.addEventListener('drop', (event) => {
      if (!dragging || dragging === buffer) return;
      event.preventDefault();
      event.stopPropagation();
      const box = tab.getBoundingClientRect();
      const at = buffers.indexOf(buffer) + (event.clientX > box.left + box.width / 2 ? 1 : 0);
      api.move(dragging, at);
    });

    tab.addEventListener('dragend', () => {
      dragging = null;
      unmark();
    });
  }

  // Dropping past the last tab moves it to the end — the strip is a target too,
  // otherwise the only way to move a tab rightmost is to aim at half a tab.
  bar.addEventListener('dragover', (event) => {
    if (!dragging) return;
    event.preventDefault();
    unmark();
    bar.lastElementChild?.classList.add('drop-after');
  });
  bar.addEventListener('drop', (event) => {
    if (!dragging) return;
    event.preventDefault();
    api.move(dragging, buffers.length);
  });

  function render() {
    bar.replaceChildren(
      ...buffers.map((buffer) => {
        const tab = document.createElement('div');
        tab.className = [
          'tab',
          buffer === active ? 'active' : '',
          // Italic, the way an editor marks a tab you are only passing through.
          buffer.preview ? 'preview' : '',
        ]
          .filter(Boolean)
          .join(' ');
        tab.title =
          buffer.workspacePath ??
          (buffer.handle ? buffer.name : `${buffer.name} — not saved to a file yet`);

        const label = document.createElement('span');
        label.className = 'tab-name';
        label.textContent = buffer.name;
        tab.append(label);

        if (isDirty(buffer)) {
          const dot = document.createElement('span');
          dot.className = 'tab-dirty';
          dot.textContent = '●';
          dot.title = 'unsaved changes';
          tab.append(dot);
        }

        const close = document.createElement('button');
        close.className = 'tab-close';
        close.textContent = '✕';
        close.title = 'Close (Alt+W)';
        close.addEventListener('click', (event) => {
          event.stopPropagation();
          api.close(buffer);
        });
        tab.append(close);

        tab.addEventListener('click', () => api.activate(buffer));
        makeDraggable(tab, buffer);
        return tab;
      }),
    );
  }

  const api = {
    get active() {
      return active;
    },

    get all() {
      return buffers;
    },

    /**
     * `handle` is a File System Access handle (ad-hoc file); `workspacePath` is a
     * path relative to the open folder. A buffer has at most one of the two, and
     * that is what decides where Save writes.
     */
    open({ name, text = '', handle = null, workspacePath = null } = {}) {
      // Opening a workspace file that is already open should focus it, not
      // duplicate it.
      if (workspacePath) {
        const existing = buffers.find((b) => b.workspacePath === workspacePath);
        if (existing) {
          api.activate(existing);
          return existing;
        }
      }

      sequence += 1;
      const doc = CodeMirror.Doc(text);
      const buffer = {
        id: sequence,
        name: name ?? `untitled-${sequence}.sql`,
        doc,
        handle,
        workspacePath,
        generation: doc.changeGeneration(),
        target: null,
        /** The tab's own result, parked here while another tab is on screen. */
        result: null,
      };
      buffers.push(buffer);
      api.activate(buffer);
      return buffer;
    },

    /**
     * The one reusable tab for looking at a table.
     *
     * Reused rather than opened afresh each time: clicking through ten tables
     * should leave you with one tab, not ten. It is also never dirty, because
     * nobody typed it — closing it must not ask whether to save.
     */
    openPreview({ name, text }) {
      const existing = buffers.find((buffer) => buffer.preview);
      const buffer = existing ?? api.open({ name, text });
      buffer.preview = true;

      if (existing) {
        existing.name = name;
        existing.doc.setValue(text);
        existing.result = null;
        api.activate(existing);
      }
      buffer.generation = buffer.doc.changeGeneration();
      render();
      return buffer;
    },

    /** Put `buffer` before whatever is at `at` today. Used by the tab drag. */
    move(buffer, at) {
      const from = buffers.indexOf(buffer);
      if (from === -1) return;
      const next = reorder(buffers, from, at);
      // In place: `all` hands this array out and app.js holds on to it.
      buffers.splice(0, buffers.length, ...next);
      dragging = null;
      unmark();
      render();
    },

    activate(buffer) {
      if (active === buffer) return;
      active = buffer;
      render();
      onActivate(buffer);
    },

    close(buffer) {
      if (isDirty(buffer) && !confirm(`${buffer.name} has unsaved changes. Close it?`)) {
        return;
      }
      const index = buffers.indexOf(buffer);
      buffers.splice(index, 1);

      // Closing the last tab is allowed, and lands you back on the start screen
      // — the same place alkyon opens on. Forcing an empty query tab instead
      // would be answering a question you did not ask.
      if (buffers.length === 0) {
        active = null;
        render();
        onActivate(null);
        return;
      }
      if (active === buffer) {
        active = null;
        api.activate(buffers[Math.min(index, buffers.length - 1)]);
      } else {
        render();
      }
    },

    /** Called after a successful save: the buffer is now clean, and may be renamed. */
    markSaved(buffer, { name, handle } = {}) {
      if (name) buffer.name = name;
      if (handle) buffer.handle = handle;
      buffer.generation = buffer.doc.changeGeneration();
      render();
      onDirtyChange?.();
    },

    /** Re-render the dirty markers; cheap enough to call on every edit. */
    refresh: render,

    isDirty,
    anyDirty: () => buffers.some(isDirty),
  };

  return api;
}
