// The bundled grid, wrapped in a small imperative API.
//
// Alkyon's UI is plain ES modules with no build step, and glide-data-grid is
// React. Rather than let React leak into the app, everything React is sealed
// inside this bundle and the outside world gets four methods on a handle.
//
// The important one is `getCell`: glide asks for the cells it is about to draw,
// so the host keeps its rows exactly as they arrived off the socket. Nothing is
// copied into a row object, which is what makes a million rows cost what a
// million rows should.

import * as React from 'react';
import { createRoot } from 'react-dom/client';
import { DataEditor, GridCellKind } from '@glideapps/glide-data-grid';

/** An empty cell that is never drawn — glide asks before a result exists. */
const BLANK = { kind: GridCellKind.Text, data: '', displayData: '', allowOverlay: false };

function Controller({ bind }) {
  const [state, setState] = React.useState({
    columns: [],
    rows: 0,
    theme: {},
    // Bumped by the host to force a redraw after the data behind `getCell`
    // changed without its shape changing — a sort, for instance.
    generation: 0,
  });

  // Held in a ref rather than in state: it changes on every result and glide
  // calls it thousands of times a second, so it must not be a render trigger.
  const source = React.useRef(() => BLANK);
  const header = React.useRef(() => {});
  const editor = React.useRef(null);

  React.useEffect(() => {
    bind({
      set({ columns, rows, getCell, theme, onHeaderClicked }) {
        if (getCell) source.current = getCell;
        if (onHeaderClicked) header.current = onHeaderClicked;
        setState((previous) => ({
          columns: columns ?? previous.columns,
          rows: rows ?? previous.rows,
          theme: theme ?? previous.theme,
          generation: previous.generation + 1,
        }));
      },
      /** Repaint without changing the shape — after sorting in place. */
      redraw() {
        setState((previous) => ({ ...previous, generation: previous.generation + 1 }));
        editor.current?.updateCells?.([]);
      },
    });
  }, [bind]);

  const getCellContent = React.useCallback((cell) => {
    const value = source.current(cell[0], cell[1]);
    return value ?? BLANK;
  }, []);

  const onHeaderClicked = React.useCallback((index) => header.current(index), []);

  const onColumnResize = React.useCallback((column, width) => {
    setState((previous) => ({
      ...previous,
      columns: previous.columns.map((c) => (c.id === column.id ? { ...c, width } : c)),
    }));
  }, []);

  // Measured, not `100%`.
  //
  // glide sizes its canvas from the width and height it is given, and a
  // percentage only resolves if every ancestor has a definite height — which is
  // one CSS change away from silently becoming a zero-height grid. Watching the
  // container and handing over pixels cannot fail that way.
  const [box, setBox] = React.useState({ width: 0, height: 0 });
  const frame = React.useRef(null);

  React.useLayoutEffect(() => {
    const element = frame.current;
    if (!element) return undefined;

    const measure = () => {
      const { width, height } = element.getBoundingClientRect();
      setBox((previous) =>
        Math.round(previous.width) === Math.round(width) &&
        Math.round(previous.height) === Math.round(height)
          ? previous
          : { width, height },
      );
    };

    // Measure now, and only *then* start observing.
    //
    // A ResizeObserver is delivered on the frame loop, and a hidden tab has no
    // frames — so gating the first render on the observer meant a grid created
    // while the window was in the background never appeared at all, and never
    // recovered until something resized it. `getBoundingClientRect` answers
    // straight away, frames or no frames.
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const ready = state.columns.length > 0 && box.width > 0 && box.height > 0;

  return React.createElement(
    'div',
    { ref: frame, style: { position: 'absolute', inset: 0 } },
    ready &&
      React.createElement(DataEditor, {
        ref: editor,
        columns: state.columns,
        rows: state.rows,
        getCellContent,
        onColumnResize,
        onHeaderClicked,
        // The row number, drawn by glide and frozen for free.
        rowMarkers: 'number',
        rowMarkerWidth: 68,
        smoothScrollX: true,
        smoothScrollY: true,
        width: box.width,
        height: box.height,
        theme: state.theme,
        // A result set is a look, not a form: no editing, no adding rows.
        onCellEdited: undefined,
        getCellsForSelection: true,
        keybindings: { search: false },
      }),
  );
}

/**
 * Mount a grid into `element`.
 *
 * `getCell(column, row)` returns a glide cell, or null for a blank one. It is
 * called only for what is on screen.
 */
function create(element) {
  let handle = null;
  const pending = [];
  const bind = (api) => {
    handle = api;
    while (pending.length) handle.set(pending.shift());
  };

  const root = createRoot(element);
  root.render(React.createElement(Controller, { bind }));

  return {
    set(next) {
      if (handle) handle.set(next);
      else pending.push(next);
    },
    redraw() {
      handle?.redraw();
    },
    destroy() {
      // Asynchronous on purpose: React refuses to unmount while it is rendering,
      // and `destroy` is called straight out of an event handler.
      setTimeout(() => root.unmount(), 0);
    },
  };
}

/** A plain text cell. Exposed so the host never imports GridCellKind itself. */
function textCell(text) {
  return { kind: GridCellKind.Text, data: text, displayData: text, allowOverlay: false };
}

window.AlkyonGrid = { create, textCell };
