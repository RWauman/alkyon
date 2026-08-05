// The sidebar's collapsible sections, after VS Code's.
//
// `aria-expanded` on the section is the single source of truth: the CSS reads it
// to hide the body and to turn the chevron, and this reads it back rather than
// keeping a parallel flag that could drift from what is on screen.

const KEY = 'alkyon.panes';

/** Collapsed state and dragged heights, so the sidebar comes back as you left it. */
function load() {
  try {
    return JSON.parse(localStorage.getItem(KEY)) ?? {};
  } catch {
    // A corrupt entry is not worth failing the sidebar over.
    return {};
  }
}

function save(state) {
  try {
    localStorage.setItem(KEY, JSON.stringify(state));
  } catch {
    // Private mode, or a full quota. The sidebar still works, it just forgets.
  }
}

/**
 * @param {HTMLElement} container the sidebar
 * @param {() => void} [onResize] told when a section's height changed
 */
export function createPanes(container, onResize) {
  const panes = [...container.querySelectorAll('.pane')];
  const splits = [...container.querySelectorAll('.pane-split')];
  const state = load();

  function persist() {
    const next = {};
    for (const pane of panes) {
      next[pane.id] = {
        open: pane.getAttribute('aria-expanded') === 'true',
        // Only a dragged height is worth keeping; `''` means "follow the content".
        height: pane.style.flexBasis || '',
      };
    }
    save(next);
  }

  /**
   * A divider is draggable whenever the section **above** it is open. That is the
   * whole condition: it resizes the one above, and what is below only has to make
   * room, which the leftover space at the bottom of the pane can do just as well
   * as another open section.
   *
   * It used to also require an open section below, which meant folding the Folder
   * section quietly killed the only divider that could resize Sources.
   */
  function paintSplits() {
    for (const split of splits) {
      const above = split.previousElementSibling;
      const usable = above?.getAttribute('aria-expanded') === 'true';
      split.classList.toggle('inert', !usable);
    }
  }

  function setOpen(pane, open) {
    pane.setAttribute('aria-expanded', String(open));
    // A pinned height means nothing while shut, and keeping it would make the
    // section reopen at a size that no longer suits what is now inside it.
    if (!open) pane.style.flex = '';
    paintSplits();
    persist();
    onResize?.();
  }

  for (const pane of panes) {
    const head = pane.querySelector('.pane-head[data-toggle]');
    if (!head) continue;

    const toggle = () => setOpen(pane, pane.getAttribute('aria-expanded') !== 'true');

    head.addEventListener('click', (event) => {
      // The header also holds actions — `+`, `Open…`, `↻`. Those are not the
      // chevron, and clicking one must not fold the section under the cursor.
      if (event.target.closest('button')) return;
      toggle();
    });
    head.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        toggle();
      }
    });
  }

  for (const split of splits) {
    split.addEventListener('pointerdown', (event) => {
      if (split.classList.contains('inert')) return;
      const above = split.previousElementSibling;
      if (!above) return;

      event.preventDefault();
      split.classList.add('dragging');
      split.setPointerCapture(event.pointerId);

      const top = above.getBoundingClientRect().top;
      const headers = [...container.querySelectorAll('.pane-head')].reduce(
        (total, head) => total + head.getBoundingClientRect().height,
        0,
      );
      // Leave room for every header plus a usable strip of whatever is below.
      const most = container.getBoundingClientRect().height - headers - 40;

      const move = (moved) => {
        const height = Math.min(Math.max(moved.clientY - top, 28), Math.max(most, 28));
        // Fixed basis, no grow, no shrink: from here the section holds the height
        // it was given rather than the height of its content.
        above.style.flex = `0 0 ${Math.round(height)}px`;
        onResize?.();
      };

      const up = () => {
        split.classList.remove('dragging');
        split.removeEventListener('pointermove', move);
        split.removeEventListener('pointerup', up);
        persist();
      };

      split.addEventListener('pointermove', move);
      split.addEventListener('pointerup', up);
    });

    // Back to following its content, the way a double-click resets a column.
    split.addEventListener('dblclick', () => {
      const above = split.previousElementSibling;
      if (!above) return;
      above.style.flex = '';
      persist();
      onResize?.();
    });
  }

  // Restore last session's shape before anything is painted over it.
  for (const pane of panes) {
    const saved = state[pane.id];
    if (!saved) continue;
    pane.setAttribute('aria-expanded', String(saved.open !== false));
    if (saved.open !== false && saved.height) pane.style.flex = `0 0 ${saved.height}`;
  }
  paintSplits();

  return {
    /** Open a section, for a shortcut that needs what is inside it. */
    reveal(id) {
      const pane = document.getElementById(id);
      if (pane && pane.getAttribute('aria-expanded') !== 'true') setOpen(pane, true);
    },
  };
}
