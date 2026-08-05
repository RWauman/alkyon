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

import {
  anyActive,
  applyFilters,
  dateTree,
  distinctValues,
  isActive,
  isCovered,
  isPartial,
  isTemporal,
  MAX_DISTINCT,
  numericBounds,
  toggleDatePrefix,
} from './filter.js';

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
  //
  // The padding also has to cover what the *header* draws over the title: the
  // menu indicator glide puts there for a filterable column, plus the sort arrow
  // and the filter mark this adds to the title itself. Without the allowance a
  // column as narrow as `object_id` loses its own name to the arrow.
  return Math.min(Math.max(Math.round(widest * 7.6) + 46, 84), 420);
}

export class ResultGrid {
  /** @param {(text: string) => void} [onFilterChange] told what the filter now shows */
  constructor(element, onFilterChange) {
    this.element = element;
    this.handle = null;
    this.host = null;
    this.onFilterChange = onFilterChange;
    /** Column metadata from the server. */
    this.meta = [];
    /** Raw rows, exactly as received. */
    this.rows = [];
    /** Row indices in display order, or null when neither filtered nor sorted. */
    this.view = null;
    this.order = null;
    /** Column index → filter text, as typed. */
    this.filters = {};
    this.popover = null;
    /** Redraws the open popover's list after the selection changed. */
    this.repaintChoices = null;
    /** Which date branches are unfolded, kept across a repaint of the list. */
    this.expanded = new Set();
    this.primed = false;
  }

  /** Start a new result set. */
  reset(columns) {
    this.destroy();
    this.meta = columns;
    this.rows = [];
    this.view = null;
    this.order = null;
    this.filters = {};
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

  /**
   * Called once the last batch has arrived.
   *
   * Through `rebuild` rather than straight to `show`, because a filter typed
   * while the page was still streaming has only seen part of it.
   */
  finish() {
    this.rebuild();
  }

  /**
   * Begin a page. The rows already here are left alone, not emptied: whoever
   * called [`take`] owns that array now, and appending to it again would rewrite
   * a page someone is still holding.
   */
  newPage() {
    this.rows = [];
    this.primed = false;
    this.clearFilters();
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
   * The sort and the filters go with the page they were applied to. Carrying
   * either over would mean the same arrow, or the same "3 of 50 000", describing
   * a different set of rows.
   */
  replace(rows) {
    this.rows = rows;
    this.view = null;
    this.order = null;
    this.primed = true;
    this.clearFilters({ quiet: true });
    this.show(rows.length);
  }

  show(count) {
    if (!this.handle) return;
    this.handle.set({
      columns: this.meta.map((column, index) => ({
        id: `c${index}`,
        title: this.titleFor(column, index),
        width: widthFor(column.name, this.rows, index),
        // Draws the indicator that opens the filter popover.
        hasMenu: true,
      })),
      // A filter or a sort replaces the row count with the length of the view;
      // `count` is only what a still-streaming page has painted so far.
      rows: this.view ? this.view.length : count,
      getCell: (column, row) => {
        const source = this.view ? this.rows[this.view[row]] : this.rows[row];
        return window.AlkyonGrid.textCell(display(source?.[column]));
      },
      onHeaderClicked: (index) => this.sortBy(index),
      onHeaderMenuClick: (index, bounds) => this.openFilter(index, bounds),
      theme: gridTheme(),
    });
  }

  /** The arrow and the funnel live in the title: glide draws headers itself. */
  titleFor(column, index) {
    const filtered = isActive(this.filters[index]) ? ' ⌕' : '';
    if (this.order?.index !== index) return `${column.name}${filtered}`;
    return `${column.name}${filtered} ${this.order.ascending ? '▲' : '▼'}`;
  }

  /**
   * Recompute the display order: filter first, then sort what survived.
   *
   * Both work over an index array rather than over the rows. The rows stay in
   * arrival order, which is what makes either one reversible, lets them compose
   * without copying, and keeps a million rows costing a million rows.
   */
  rebuild(count = this.rows.length) {
    const kept = applyFilters(this.rows, this.filters, this.meta);

    if (this.order) {
      const base = kept ?? this.rows.map((_, i) => i);
      const at = this.order.index;
      const sign = this.order.ascending ? 1 : -1;
      const compare = comparatorFor(this.meta[at]?.logical);
      base.sort((a, b) => sign * compare(this.rows[a][at], this.rows[b][at]));
      this.view = base;
    } else {
      this.view = kept;
    }

    this.show(count);
    this.report();
  }

  /** Say what the filters currently hide, or nothing when none do. */
  report() {
    if (!this.onFilterChange) return;
    this.onFilterChange(
      anyActive(this.filters) && this.view
        ? `${this.view.length.toLocaleString()} of ${this.rows.length.toLocaleString()} rows on this page`
        : '',
    );
  }

  sortBy(index) {
    const ascending = !(this.order?.index === index && this.order.ascending);
    this.order = { index, ascending };
    this.rebuild();
  }

  /** Merge a change into one column's filter, dropping it once nothing is set. */
  setFilter(index, change) {
    const filter = { ...(this.filters[index] ?? { text: '' }), ...change };
    if (isActive(filter)) this.filters[index] = filter;
    else delete this.filters[index];
    this.rebuild();
  }

  clearFilters({ quiet = false } = {}) {
    this.filters = {};
    this.closeFilter();
    if (!quiet) this.report();
  }

  /**
   * A small popover under the column's menu indicator.
   *
   * Plain DOM on top of the canvas rather than anything glide draws: glide
   * reports where the indicator ended up and has no opinion about the menu, which
   * is the right division — a text input belongs to the browser.
   */
  openFilter(index, bounds) {
    this.closeFilter();
    const column = this.meta[index];
    if (!column) return;
    // Folded state belongs to the popover, not to the column: reopening on a
    // different column should not inherit which years were unfolded on the last.
    this.expanded = new Set();

    const pop = document.createElement('div');
    pop.className = 'filter-pop';

    const title = document.createElement('div');
    title.className = 'filter-title';
    title.textContent = column.name;

    const input = document.createElement('input');
    input.type = 'search';
    input.placeholder = 'contains… or > 100';
    input.value = this.filters[index] ?? '';
    input.spellcheck = false;

    const hint = document.createElement('div');
    hint.className = 'filter-hint';
    hint.textContent = 'Plain text matches anywhere. > >= < <= = != compare.';

    // Values or dates, whichever this column can offer. Rebuilt rather than
    // patched when the selection changes: the list is at most a few hundred rows
    // and reasoning about a rebuild is easier than reasoning about a diff.
    const list = document.createElement('div');
    list.className = 'filter-list';
    const paintList = () => this.paintChoices(list, index, input.value);

    const clear = document.createElement('button');
    clear.type = 'button';
    clear.textContent = 'Clear';
    clear.addEventListener('click', () => {
      input.value = '';
      delete this.filters[index];
      this.rebuild();
      this.closeFilter();
    });

    const actions = document.createElement('div');
    actions.className = 'filter-actions';
    actions.append(clear);

    // Filtered as you type: an Apply button would make every keystroke a decision
    // about whether to press it. The same text also narrows the list below, so one
    // box both filters the rows and finds the value you are looking for.
    let timer = null;
    input.addEventListener('input', () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        this.setFilter(index, { text: input.value });
        paintList();
      }, 140);
    });
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        this.closeFilter();
      } else if (event.key === 'Enter') {
        clearTimeout(timer);
        this.setFilter(index, { text: input.value });
        this.closeFilter();
      }
    });

    pop.append(title, input, hint, list, actions);
    document.body.append(pop);

    /**
     * Anchored to the indicator, then pulled back inside the window — a filter on
     * the last column would otherwise open half off screen.
     *
     * Called *before* the list is filled as well as after: positioning only once,
     * at the end, meant that anything going wrong while building the list left the
     * popover sitting in the top-left corner of the window with no explanation.
     */
    const place = () => {
      const { width } = pop.getBoundingClientRect();
      const left = Math.min(bounds.x, window.innerWidth - width - 8);
      pop.style.left = `${Math.max(8, left)}px`;
      pop.style.top = `${bounds.y + bounds.height}px`;
    };

    place();
    this.repaintChoices = paintList;
    paintList();
    place();

    // Registered on the next frame: the click that opened this one is still
    // propagating, and would close it immediately.
    const away = (event) => {
      if (!pop.contains(event.target)) this.closeFilter();
    };
    requestAnimationFrame(() => document.addEventListener('pointerdown', away));

    this.popover = { element: pop, away, timer: () => clearTimeout(timer) };
    input.focus();
    input.select();
  }

  closeFilter() {
    if (!this.popover) return;
    this.popover.timer();
    document.removeEventListener('pointerdown', this.popover.away);
    this.popover.element.remove();
    this.popover = null;
    this.repaintChoices = null;
  }

  /**
   * Fill the popover's list: a year/month/day tree for a date column, a list of
   * values with counts for anything discrete enough to list.
   *
   * `needle` is whatever is in the text box, used to narrow the *list* as well as
   * the rows — with a few hundred values, finding one is the hard part.
   */
  paintChoices(list, index, needle) {
    const column = this.meta[index];
    const chosen = this.filters[index] ?? {};
    list.replaceChildren();

    if (isTemporal(column?.logical)) {
      const { tree, skipped } = dateTree(this.rows, index);
      if (tree.length === 0) {
        list.append(this.note('no dates on this page to choose from'));
        return;
      }
      for (const year of tree) list.append(this.dateRow(index, tree, year, 0));
      if (skipped) list.append(this.note(`${skipped.toLocaleString()} row(s) with no date`));
      return;
    }

    // A numeric column gets two ends rather than a list: ticking values one by one
    // is the wrong shape for a measure, and a measure is usually all distinct
    // anyway, so the list would refuse to appear at all.
    if (NUMERIC.has(column?.logical)) {
      this.paintRange(list, index);
      return;
    }

    const { values, truncated } = distinctValues(this.rows, index);
    if (truncated) {
      list.append(
        this.note(
          `more than ${MAX_DISTINCT} distinct values — use the box above instead of a list`,
        ),
      );
      return;
    }

    const wanted = needle.trim().toLowerCase();
    const shown = wanted
      ? values.filter((entry) => entry.value.toLowerCase().includes(wanted))
      : values;
    if (shown.length === 0) {
      list.append(this.note('no value matches'));
      return;
    }

    // Both act on what is *shown*, so they compose with the text box rather than
    // reaching past it: type `be`, press None, and only the values containing "be"
    // are cleared.
    const bar = document.createElement('div');
    bar.className = 'filter-bulk';
    for (const [label, on] of [['All', true], ['None', false]]) {
      const button = document.createElement('button');
      button.type = 'button';
      button.textContent = label;
      button.title = on ? 'Tick every value shown' : 'Untick every value shown';
      button.addEventListener('click', () => {
        // Starting from the live filter, or from the two ends: `All` from nothing,
        // `None` from everything — because unticking has to start from a set that
        // holds what it is about to remove.
        const current = this.filters[index]?.include;
        const include = new Set(current ?? (on ? [] : values.map((one) => one.value)));
        for (const entry of shown) {
          if (on) include.add(entry.value);
          else include.delete(entry.value);
        }
        // Everything ticked is the same as no restriction. An *empty* set is not:
        // it means no rows, which is what None says.
        this.setFilter(index, { include: include.size === values.length ? null : include });
        // Many boxes changed at once, so here the list does have to be redrawn.
        this.repaintChoices?.();
      });
      bar.append(button);
    }
    list.append(bar);

    for (const entry of shown) {
      const row = document.createElement('label');
      row.className = 'filter-choice';

      const box = document.createElement('input');
      box.type = 'checkbox';
      // Nothing ticked means no restriction, so every box starts ticked — the
      // list reads as "these are included", which is what it does.
      box.checked = chosen.include ? chosen.include.has(entry.value) : true;
      box.addEventListener('change', () => {
        // Read the live filter rather than what was there when the list was
        // painted: ticking one box must not undo the box ticked before it.
        const current = this.filters[index]?.include;
        const include = new Set(current ?? values.map((one) => one.value));
        if (box.checked) include.add(entry.value);
        else include.delete(entry.value);
        // Everything ticked is the same as no restriction; say it that way so the
        // header stops claiming the column is filtered.
        this.setFilter(index, { include: include.size === values.length ? null : include });
        // Deliberately *not* repainted: the box already shows what was clicked,
        // and replacing the row under the cursor loses focus and makes a quick
        // second click land on a node that is no longer in the document.
      });

      const text = document.createElement('span');
      text.className = 'filter-value';
      text.textContent = entry.value;
      text.title = entry.value;

      const count = document.createElement('span');
      count.className = 'filter-count';
      count.textContent = entry.count.toLocaleString();

      row.append(box, text, count);
      list.append(row);
    }
  }

  /**
   * Two ends and two sliders, for a numeric column.
   *
   * The sliders are two ordinary `input[type=range]`, not one two-handled control:
   * the platform has no such control, and faking one with overlaid tracks costs
   * pointer maths and keyboard handling that the boxes beside them already do
   * better. The boxes are the exact answer; the sliders are how you find it.
   */
  paintRange(list, index) {
    const { min, max, counted, missing } = numericBounds(this.rows, index);
    if (min === null) {
      list.append(this.note('nothing numeric on this page to bound'));
      return;
    }

    const chosen = this.filters[index]?.range ?? {};
    // A step that gives a slider a useful travel over the data's own spread. An
    // integer column gets whole numbers; a narrow float range gets fine ones.
    const spread = max - min;
    const whole = Number.isInteger(min) && Number.isInteger(max) && spread >= 1;
    const step = whole ? 1 : spread / 1000 || 'any';

    const state = {
      min: chosen.min ?? min,
      max: chosen.max ?? max,
    };

    const rows = [];
    const push = (edge, label) => {
      const row = document.createElement('div');
      row.className = 'filter-range';

      const name = document.createElement('span');
      name.className = 'filter-range-label';
      name.textContent = label;

      const slider = document.createElement('input');
      slider.type = 'range';
      slider.min = String(min);
      slider.max = String(max);
      slider.step = String(step);
      slider.value = String(state[edge]);

      const box = document.createElement('input');
      box.type = 'number';
      box.className = 'filter-range-box';
      box.step = String(step);
      box.value = String(state[edge]);

      const apply = (raw, from) => {
        const value = Number(raw);
        if (Number.isNaN(value)) return;
        state[edge] = value;
        // The ends may not cross: dragging min past max carries max with it, which
        // is what every range control does and what stops an empty result you did
        // not ask for.
        if (edge === 'min' && state.min > state.max) state.max = state.min;
        if (edge === 'max' && state.max < state.min) state.min = state.max;

        for (const other of rows) other.sync();
        if (from !== 'slider') slider.value = String(state[edge]);
        if (from !== 'box') box.value = String(state[edge]);

        // Covering the whole spread is not a filter — say so, so the header stops
        // claiming the column is filtered.
        const bounds =
          state.min <= min && state.max >= max ? null : { min: state.min, max: state.max };
        this.setFilter(index, { range: bounds });
      };

      slider.addEventListener('input', () => apply(slider.value, 'slider'));
      box.addEventListener('input', () => apply(box.value, 'box'));

      row.append(name, slider, box);
      list.append(row);
      rows.push({
        sync() {
          slider.value = String(state[edge]);
          box.value = String(state[edge]);
        },
      });
    };

    push('min', 'from');
    push('max', 'to');

    const reset = document.createElement('button');
    reset.type = 'button';
    reset.textContent = 'Full range';
    reset.addEventListener('click', () => {
      state.min = min;
      state.max = max;
      for (const row of rows) row.sync();
      this.setFilter(index, { range: null });
    });
    const bar = document.createElement('div');
    bar.className = 'filter-bulk';
    bar.append(reset);
    list.append(bar);

    list.append(
      this.note(
        `${counted.toLocaleString()} value(s) between ${min.toLocaleString()} and ${max.toLocaleString()}` +
          (missing ? `, ${missing.toLocaleString()} without one` : ''),
      ),
    );
  }

  /** One year, month or day, with its children folded underneath. */
  dateRow(index, tree, node, depth) {
    const wrapper = document.createElement('div');
    wrapper.className = 'filter-date';
    wrapper.style.paddingLeft = `${depth * 0.9}rem`;

    const row = document.createElement('div');
    row.className = 'filter-choice';

    const twisty = document.createElement('span');
    twisty.className = node.children.length ? 'twisty' : 'twisty leaf';
    twisty.textContent = '▸';

    const box = document.createElement('input');
    box.type = 'checkbox';
    const prefixes = this.filters[index]?.prefixes;
    box.checked = isCovered(prefixes, node.prefix);
    box.indeterminate = isPartial(prefixes, node.prefix);
    box.addEventListener('change', (event) => {
      event.stopPropagation();
      this.setFilter(index, {
        prefixes: toggleDatePrefix(this.filters[index]?.prefixes, tree, node.prefix, box.checked),
      });
      this.repaintChoices?.();
    });

    const label = document.createElement('span');
    label.className = 'filter-value';
    // Only the part this level adds: `2022`, then `03`, then `15`.
    label.textContent = depth === 0 ? node.prefix : node.prefix.slice(node.prefix.length - 2);

    const count = document.createElement('span');
    count.className = 'filter-count';
    count.textContent = node.count.toLocaleString();

    row.append(twisty, box, label, count);

    const children = document.createElement('div');
    // Whether this branch is open outlives the list being rebuilt: ticking a month
    // repaints, and a repaint that folded the branch you were working in would
    // close the tree under your hand on every click.
    const open = this.expanded.has(node.prefix);
    children.hidden = !open;
    if (node.children.length) {
      twisty.textContent = open ? '▾' : '▸';
      let built = false;
      const build = () => {
        // Built on first open: a year of days is 365 rows nobody asked for yet.
        if (built) return;
        built = true;
        for (const child of node.children) {
          children.append(this.dateRow(index, tree, child, depth + 1));
        }
      };
      if (open) build();

      const toggle = () => {
        children.hidden = !children.hidden;
        twisty.textContent = children.hidden ? '▸' : '▾';
        if (children.hidden) this.expanded.delete(node.prefix);
        else this.expanded.add(node.prefix);
        build();
      };
      twisty.addEventListener('click', toggle);
      label.addEventListener('click', toggle);
    }

    wrapper.append(row, children);
    return wrapper;
  }

  note(text) {
    const note = document.createElement('div');
    note.className = 'filter-note';
    note.textContent = text;
    return note;
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
    this.closeFilter();
    this.rows = [];
    this.view = null;
    this.filters = {};
    this.primed = false;
    this.handle?.destroy();
    this.handle = null;
    this.host = null;
    this.element.replaceChildren();
    // The empty-pane watermark keys off this class, so do not leave it behind.
    this.element.classList.remove('has-grid');
  }
}
