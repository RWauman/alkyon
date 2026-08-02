// The result grid.
//
// Rows arrive in batches while the query is still running, and the grid holds on
// to them until the end rather than feeding each batch to Tabulator as it lands.
//
// That is not an optimisation, it is the difference between working and not.
// `table.addData()` reprocesses the whole dataset on every call, so filling a
// grid batch by batch is quadratic — measured on 19 columns:
//
//     5 000 rows   in 500-row batches   5 480 ms      one setData    469 ms
//    10 000 rows   in 500-row batches  17 616 ms
//   100 000 rows   in 500-row batches   never finished (froze the tab)
//   100 000 rows                                      one setData  1 733 ms
//
// Doubling the rows tripled the time. Loaded in one call it is linear, about
// 10 µs a row, and 100 000 rows land in under two seconds.

/**
 * Tabulator writes a formatter's return value as HTML, so returning an element
 * with `textContent` set is what keeps a cell containing `<script>` inert.
 */
function formatCell(cell) {
  const value = cell.getValue();
  const span = document.createElement('span');
  if (value === null || value === undefined) {
    span.className = 'cell-null';
    span.textContent = 'NULL';
  } else if (typeof value === 'object') {
    span.textContent = JSON.stringify(value);
  } else {
    span.textContent = String(value);
  }
  return span;
}

/**
 * Rows shown while the query is still running, so a long one is not answered by
 * an empty pane. Small enough that painting it twice costs nothing.
 */
const PREVIEW_ROWS = 500;

/**
 * The leading row-number column.
 *
 * A fixed width rather than one measured from the data: the grid renders only the
 * visible rows, so sizing to content would fit `1`–`40` and then clip once you
 * scrolled to six digits. Frozen, so the number stays put while you scroll a wide
 * result sideways — which is the whole point of having it.
 *
 * It counts *displayed* position, so sorting renumbers. That is what a row number
 * means in a grid; the row's identity is whatever key the data carries.
 */
function rowNumberColumn() {
  return {
    title: '#',
    field: '__row',
    formatter: 'rownum',
    hozAlign: 'right',
    headerHozAlign: 'right',
    headerSort: false,
    resizable: false,
    frozen: true,
    width: 68,
    cssClass: 'rownum',
  };
}

export class ResultGrid {
  constructor(element) {
    this.element = element;
    this.table = null;
    this.ready = null;
    /** Every row received so far, handed to Tabulator in one go at the end. */
    this.rows = [];
    this.primed = false;
    this.width = 0;
  }

  /**
   * Start a new result set. A fresh table per query avoids reconciling column
   * definitions, and duplicate column names stay distinct because the field is
   * the position, not the name.
   */
  reset(columns) {
    this.destroy();
    this.width = columns.length;
    this.table = new Tabulator(this.element, {
      height: '100%',
      layout: 'fitDataStretch',
      renderVertical: 'virtual',
      placeholder: 'no rows',
      columnDefaults: { formatter: formatCell, headerHozAlign: 'left' },
      columns: [rowNumberColumn(), ...columns.map((column, index) => ({
        field: `c${index}`,
        title: column.name,
        headerTooltip: `${column.name} — ${column.type_name}`,
      }))],
      data: [],
    });
    // Tabulator 6 rejects addData before the table is built.
    this.ready = new Promise((resolve) => this.table.on('tableBuilt', resolve));
  }

  append(rows) {
    for (const row of rows) {
      const record = {};
      for (let i = 0; i < this.width; i += 1) record[`c${i}`] = row[i];
      this.rows.push(record);
    }
    // One early paint, so a query that streams for a while shows its shape
    // instead of an empty pane. After that the grid waits for the end — every
    // extra load costs the whole dataset again.
    if (!this.primed && this.rows.length >= PREVIEW_ROWS) {
      this.primed = true;
      this.load(this.rows.slice(0, PREVIEW_ROWS));
    }
  }

  /** Hand `records` to Tabulator, guarding against a query that moved on. */
  async load(records) {
    if (!this.table) return;
    const table = this.table;
    await this.ready;
    if (this.table === table) await table.setData(records);
  }

  /**
   * Called once the last batch has arrived: the single load that matters.
   *
   * `redraw(true)` is what sizes the columns — `fitDataStretch` measures against
   * the data, and at `reset` time there was none, so without it every column
   * stays as narrow as its header.
   */
  async finish() {
    await this.load(this.rows);
    this.table?.redraw(true);
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
    this.primed = false;
    if (this.table) {
      this.table.destroy();
      this.table = null;
      this.ready = null;
    }
    this.element.replaceChildren();
    // The empty-pane watermark keys off this class, so do not leave it behind.
    this.element.classList.remove('tabulator');
  }
}
