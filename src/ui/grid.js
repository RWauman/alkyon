// The result grid, drawn on a canvas by glide-data-grid.
//
// Two properties matter here, and they are the same property twice:
//
//   1. Rows are kept **exactly as they arrive off the socket** — arrays of JSON
//      values, no per-row object, no copy. glide asks for the cells it is about
//      to paint and nothing else, so holding a million rows costs a million rows
//      and not a million row components.
//
//   2. There is no ingestion step. Handing the grid a result is telling it how
//      many rows exist; it reads what it needs, when it needs it.
//
// The predecessor built one object per row and fed Tabulator batch by batch,
// which reprocessed the whole dataset on every call — 10 000 rows took 17.6
// seconds and 100 000 never finished at all.

/**
 * Rows painted while a query is still streaming, so a long one is not answered
 * by an empty pane.
 */
const PREVIEW_ROWS = 500;

/** Read a theme custom property off the document. */
function css(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/**
 * glide paints on a canvas, so it cannot inherit anything from the stylesheet —
 * every colour has to be handed over, and handed over again when the theme
 * changes.
 */
function gridTheme() {
  return {
    accentColor: css('--accent'),
    accentLight: css('--accent-soft'),
    textDark: css('--fg'),
    textMedium: css('--fg'),
    textLight: css('--fg-muted'),
    textHeader: css('--fg'),
    textHeaderSelected: css('--bg'),
    textBubble: css('--fg'),
    bgCell: css('--bg'),
    bgCellMedium: css('--bg-raised'),
    bgHeader: css('--bg-sunken'),
    bgHeaderHovered: css('--bg-raised'),
    bgHeaderHasFocus: css('--bg-raised'),
    bgBubble: css('--bg-raised'),
    bgBubbleSelected: css('--bg-raised'),
    bgIconHeader: css('--fg-muted'),
    fgIconHeader: css('--bg'),
    borderColor: css('--line'),
    horizontalBorderColor: css('--line'),
    drilldownBorder: css('--line'),
    linkColor: css('--accent'),
    fontFamily: css('--mono'),
    baseFontStyle: '12px',
    headerFontStyle: '600 12px',
    cellHorizontalPadding: 8,
  };
}

/** What a cell shows. NULL is spelled out, because empty and absent differ. */
function display(value) {
  if (value === null || value === undefined) return 'NULL';
  if (typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

/** Logical types whose values must be ordered as numbers, not as text. */
const NUMERIC = new Set(['int', 'float', 'decimal']);

/**
 * A comparator for a column, chosen from what the server said the column holds.
 *
 * The type matters and cannot be guessed from the value: `numeric` and `decimal`
 * arrive as **strings**, deliberately, so their digits survive a round trip
 * through JSON. Comparing those as text sorts 1002.75 before 13.37, which is
 * what a sort on a money column did until someone looked at it.
 *
 * NULLs go last whichever direction you sort — they are the absence of an
 * answer, not the smallest one.
 */
function comparatorFor(logical) {
  const numeric = NUMERIC.has(logical);
  return (a, b) => {
    const missing = (v) => v === null || v === undefined;
    if (missing(a)) return missing(b) ? 0 : 1;
    if (missing(b)) return -1;

    if (numeric) {
      // Beyond 2^53 two adjacent values may tie rather than order. Exact
      // ordering there would mean comparing digit strings by hand; ties in the
      // eighteenth significant figure are not what a sort is for.
      const [x, y] = [Number(a), Number(b)];
      if (!Number.isNaN(x) && !Number.isNaN(y)) return x - y;
    }

    const [x, y] = [display(a), display(b)];
    // Deliberately not `localeCompare`: it is an order of magnitude slower, and
    // this runs over every row of a result that may have a million of them.
    return x < y ? -1 : x > y ? 1 : 0;
  };
}

/** Width in pixels for a column, measured against a sample of the data. */
function widthFor(name, rows, index) {
  const sample = Math.min(rows.length, 200);
  let widest = name.length;
  for (let r = 0; r < sample; r += 1) {
    const length = display(rows[r][index]).length;
    if (length > widest) widest = length;
  }
  // 7.6px per character at 12px monospace, plus padding, clamped so one long
  // JSON blob cannot push every other column off screen.
  return Math.min(Math.max(Math.round(widest * 7.6) + 26, 72), 420);
}

export class ResultGrid {
  constructor(element) {
    this.element = element;
    this.handle = null;
    this.host = null;
    /** Column metadata from the server. */
    this.meta = [];
    /** Raw rows, exactly as received. */
    this.rows = [];
    /** Row indices in display order, or null while unsorted. */
    this.view = null;
    this.order = null;
    this.primed = false;
  }

  /** Start a new result set. */
  reset(columns) {
    this.destroy();
    this.meta = columns;
    this.rows = [];
    this.view = null;
    this.order = null;
    this.primed = false;

    // A fresh node per result: unmounting a React root is deferred, and mounting
    // a new one on a container that still has one is asking for trouble.
    this.host = document.createElement('div');
    this.host.className = 'grid-host';
    this.element.replaceChildren(this.host);
    this.element.classList.add('has-grid');
    this.handle = window.AlkyonGrid.create(this.host);
  }

  append(rows) {
    // The whole point: no transformation, no allocation per row.
    for (const row of rows) this.rows.push(row);

    // One early paint so a long query shows its shape rather than an empty pane.
    if (!this.primed && this.rows.length >= PREVIEW_ROWS) {
      this.primed = true;
      this.show(PREVIEW_ROWS);
    }
  }

  /** Called once the last batch has arrived. */
  finish() {
    this.show(this.rows.length);
  }

  /**
   * Begin a page. The rows already here are left alone, not emptied: whoever
   * called [`take`] owns that array now, and appending to it again would rewrite
   * a page someone is still holding.
   */
  newPage() {
    this.rows = [];
    this.primed = false;
  }

  /**
   * Hand this page's rows over. The caller owns them from here; the grid goes on
   * showing them until the next [`newPage`].
   */
  take() {
    return this.rows;
  }

  /**
   * Show rows the caller already had: a page being revisited.
   *
   * The sort goes with the page it was applied to. Carrying it over would mean
   * silently re-sorting a different set of rows under the same arrow.
   */
  replace(rows) {
    this.rows = rows;
    this.view = null;
    this.order = null;
    this.primed = true;
    this.show(rows.length);
  }

  show(count) {
    if (!this.handle) return;
    this.handle.set({
      columns: this.meta.map((column, index) => ({
        id: `c${index}`,
        title: this.titleFor(column, index),
        width: widthFor(column.name, this.rows, index),
      })),
      rows: count,
      getCell: (column, row) => {
        const source = this.view ? this.rows[this.view[row]] : this.rows[row];
        return window.AlkyonGrid.textCell(display(source?.[column]));
      },
      onHeaderClicked: (index) => this.sortBy(index),
      theme: gridTheme(),
    });
  }

  /** The arrow lives in the title: glide draws headers itself. */
  titleFor(column, index) {
    if (this.order?.index !== index) return column.name;
    return `${column.name} ${this.order.ascending ? '▲' : '▼'}`;
  }

  /**
   * Sort on a column, in place over an index array rather than over the rows —
   * the rows stay in arrival order, which is what makes sorting reversible and
   * cheap in memory.
   */
  sortBy(index) {
    const ascending = !(this.order?.index === index && this.order.ascending);
    this.order = { index, ascending };
    const sign = ascending ? 1 : -1;
    const compare = comparatorFor(this.meta[index]?.logical);

    this.view = this.rows.map((_, i) => i);
    this.view.sort((a, b) => sign * compare(this.rows[a][index], this.rows[b][index]));
    this.show(this.rows.length);
  }

  /** Repaint with the current theme — the canvas inherits nothing. */
  setTheme() {
    this.handle?.set({ theme: gridTheme() });
  }

  /** Show a message instead of a grid — for DDL, or for a failed statement. */
  message(text) {
    this.destroy();
    const note = document.createElement('div');
    note.className = 'muted';
    note.style.padding = '.6rem';
    note.textContent = text;
    this.element.replaceChildren(note);
  }

  destroy() {
    this.rows = [];
    this.view = null;
    this.primed = false;
    this.handle?.destroy();
    this.handle = null;
    this.host = null;
    this.element.replaceChildren();
    // The empty-pane watermark keys off this class, so do not leave it behind.
    this.element.classList.remove('has-grid');
  }
}
