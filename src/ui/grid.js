// The result grid. Rows arrive in batches while the query is still running, so
// the grid buffers them and flushes once a frame instead of once a batch.

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

export class ResultGrid {
  constructor(element) {
    this.element = element;
    this.table = null;
    this.ready = null;
    this.buffer = [];
    this.scheduled = false;
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
      columns: columns.map((column, index) => ({
        field: `c${index}`,
        title: column.name,
        headerTooltip: `${column.name} — ${column.type_name}`,
      })),
      data: [],
    });
    // Tabulator 6 rejects addData before the table is built.
    this.ready = new Promise((resolve) => this.table.on('tableBuilt', resolve));
  }

  append(rows) {
    for (const row of rows) {
      const record = {};
      for (let i = 0; i < this.width; i += 1) record[`c${i}`] = row[i];
      this.buffer.push(record);
    }
    if (this.scheduled) return;
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      this.flush();
    });
  }

  async flush() {
    if (!this.table || this.buffer.length === 0) return;
    const batch = this.buffer;
    this.buffer = [];
    const table = this.table;
    await this.ready;
    // A new query may have replaced the table while we were waiting.
    if (this.table === table) await table.addData(batch);
  }

  /**
   * Called once the last batch has arrived. `fitDataStretch` measures columns
   * against the data, and at `reset` time there was none — without this every
   * column stays as narrow as its header.
   */
  async finish() {
    await this.flush();
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
    this.buffer = [];
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
