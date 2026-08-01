// Open buffers and the tab bar.
//
// One CodeMirror `Doc` per buffer, swapped into the single editor — that keeps
// undo history, cursor and scroll position per tab for free. Each buffer also
// remembers which source and database it was last run against, so switching tabs
// switches target the way SSMS and DataGrip do.

export function createBuffers(bar, { onActivate, onDirtyChange }) {
  /** @type {Array<{id:number,name:string,doc:any,handle:any,generation:number,target:object|null}>} */
  const buffers = [];
  let active = null;
  let sequence = 0;

  const isDirty = (buffer) => !buffer.doc.isClean(buffer.generation);

  function render() {
    bar.replaceChildren(
      ...buffers.map((buffer) => {
        const tab = document.createElement('div');
        tab.className = buffer === active ? 'tab active' : 'tab';
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
      };
      buffers.push(buffer);
      api.activate(buffer);
      return buffer;
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

      // Never leave the workbench with no buffer at all.
      if (buffers.length === 0) {
        active = null;
        api.open();
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
