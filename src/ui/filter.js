// Column filters for the result grid.
//
// Kept as pure functions away from the grid so the matching rules can be tested
// without a canvas, a socket or a browser.
//
// **A filter applies to the page in memory, not to the result.** That is the same
// scope as the sort, and for the same reason: the rows the browser holds are the
// ones it can reorder or hide. Pushing a filter into the SQL would be a different
// feature — it would re-run the query — and pretending this one does that would be
// worse than saying it plainly.

/** Logical types whose values are compared as numbers rather than as text. */
const NUMERIC = new Set(['int', 'float', 'decimal']);

/**
 * Operators a filter may lead with. Longest first: `>=` has to be tried before
 * `>`, or `>=5` parses as "greater than `=5`".
 */
const OPERATORS = ['>=', '<=', '!=', '<>', '=', '>', '<'];

/** How a cell reads, matching what the grid draws. */
function display(value) {
  if (value === null || value === undefined) return 'NULL';
  if (typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

/**
 * Split `">= 100"` into an operator and its operand, or `null` for a plain
 * substring search.
 */
export function parseFilter(text) {
  const trimmed = text.trim();
  if (!trimmed) return null;

  for (const operator of OPERATORS) {
    if (trimmed.startsWith(operator)) {
      const operand = trimmed.slice(operator.length).trim();
      // `>` with nothing after it is someone mid-typing, not a filter.
      if (!operand) return null;
      return { operator: operator === '<>' ? '!=' : operator, operand };
    }
  }
  return { operator: 'contains', operand: trimmed };
}

/**
 * A predicate for one column.
 *
 * Returns `null` when the text says nothing, so a cleared box costs no work
 * rather than matching everything one row at a time.
 *
 * `logical` decides whether comparisons are numeric: `decimal` arrives as a
 * *string* to keep its digits through JSON, so `>= 1000` on a money column would
 * otherwise compare text and put 999.99 above 1000.
 */
export function predicateFor(text, logical) {
  const parsed = parseFilter(text);
  if (!parsed) return null;

  const { operator, operand } = parsed;
  if (operator === 'contains') {
    const needle = operand.toLowerCase();
    return (value) => display(value).toLowerCase().includes(needle);
  }

  const numeric = NUMERIC.has(logical) && operand !== '' && !Number.isNaN(Number(operand));
  const target = numeric ? Number(operand) : operand.toLowerCase();

  return (value) => {
    // NULL is the absence of a value, so it satisfies no comparison — not even
    // `!=`, where counting it as a match would quietly turn "not 5" into
    // "not 5, plus everything unknown".
    if (value === null || value === undefined) return false;

    const left = numeric ? Number(value) : display(value).toLowerCase();
    if (numeric && Number.isNaN(left)) return false;

    switch (operator) {
      case '>=': return left >= target;
      case '<=': return left <= target;
      case '>': return left > target;
      case '<': return left < target;
      case '=': return left === target;
      case '!=': return left !== target;
      default: return true;
    }
  };
}

/**
 * Row indices that pass every active filter, in the order given.
 *
 * `filters` is column index → filter object. Nothing active returns `null`,
 * meaning "no filter" — the grid then reads its rows directly instead of through
 * an index array it would have to build for nothing.
 */
export function applyFilters(rows, filters, meta) {
  const predicates = [];
  for (const [index, filter] of Object.entries(filters)) {
    const predicate = columnPredicate(filter, meta[index]?.logical);
    if (predicate) predicates.push([Number(index), predicate]);
  }
  if (predicates.length === 0) return null;

  const kept = [];
  for (let r = 0; r < rows.length; r += 1) {
    const row = rows[r];
    let passes = true;
    for (const [index, predicate] of predicates) {
      if (!predicate(row[index])) {
        passes = false;
        break;
      }
    }
    if (passes) kept.push(r);
  }
  return kept;
}

/**
 * Whether any column carries a filter worth applying.
 *
 * A **present** `include` counts even when it is empty, and that distinction is
 * the whole point: `null` means "no restriction", an empty set means "nothing
 * passes". Treating the empty set as "no filter" is what made unticking the last
 * value silently show every row again — and, worse, leave every box ticked so
 * that ticking one more did nothing.
 */
export function anyActive(filters) {
  return Object.values(filters).some(
    (filter) =>
      parseFilter(filter?.text ?? '') !== null ||
      filter?.include instanceof Set ||
      filter?.prefixes?.size > 0 ||
      isRange(filter?.range),
  );
}

/** Whether a range bounds anything. A range with both ends open does not. */
export function isRange(range) {
  if (!range) return false;
  const bounded = (edge) => edge !== null && edge !== undefined && !Number.isNaN(edge);
  return bounded(range.min) || bounded(range.max);
}

/** Whether one column's filter is doing anything — what marks its header. */
export function isActive(filter) {
  return anyActive({ one: filter });
}

/** Logical types the year/month/day picker is offered for. */
const TEMPORAL = new Set(['date', 'timestamp', 'timestamp_tz']);

export function isTemporal(logical) {
  return TEMPORAL.has(logical);
}

/**
 * How many distinct values are worth listing.
 *
 * There is deliberately no cap on how many **rows** are read. The list describes
 * the page the grid is holding, so sampling it would make the counts wrong and the
 * set incomplete for no visible reason. What bounds the work instead is this: past
 * this many distinct values there is no list to show, so the scan stops the moment
 * it knows that — which is what keeps a unique key column in a ten-million-row page
 * from being walked to the end for nothing.
 */
export const MAX_DISTINCT = 500;

/**
 * The distinct values in one column, most frequent first.
 *
 * `truncated` means there were more distinct values than are worth listing, so the
 * caller says so rather than presenting a partial list as the whole set.
 */
export function distinctValues(rows, index, { max = MAX_DISTINCT } = {}) {
  const counts = new Map();

  for (let r = 0; r < rows.length; r += 1) {
    const key = display(rows[r][index]);
    const seen = counts.get(key);
    if (seen === undefined) {
      // One past the limit is enough to know the list is not worth building.
      if (counts.size >= max) return { values: [], truncated: true };
      counts.set(key, 1);
    } else {
      counts.set(key, seen + 1);
    }
  }

  const values = [...counts.entries()]
    .map(([value, count]) => ({ value, count }))
    .sort((a, b) => b.count - a.count || (a.value < b.value ? -1 : 1));
  return { values, truncated: false };
}

/**
 * The smallest and largest number in a column, and how many rows had one.
 *
 * `null` bounds mean the column held no number at all. Values arrive as text for
 * `decimal`, so this is also where that becomes comparable.
 */
export function numericBounds(rows, index) {
  let min = null;
  let max = null;
  let counted = 0;
  let missing = 0;

  for (let r = 0; r < rows.length; r += 1) {
    const raw = rows[r][index];
    if (raw === null || raw === undefined || raw === '') {
      missing += 1;
      continue;
    }
    const value = Number(raw);
    if (Number.isNaN(value)) {
      missing += 1;
      continue;
    }
    if (min === null || value < min) min = value;
    if (max === null || value > max) max = value;
    counted += 1;
  }
  return { min, max, counted, missing };
}

/**
 * Group a temporal column into year → month → day, from the ISO text the values
 * already arrive as.
 *
 * No date parsing anywhere: `2022-03-15` and `2022-03-15T10:30:00` both start
 * with the year, then the month, then the day, so each level is a prefix of the
 * string. That is also why filtering by one is a `startsWith` and needs no
 * calendar — and why it is safe, since ISO months and days are zero-padded and
 * cannot run into each other.
 */
export function dateTree(rows, index) {
  const years = new Map();
  let skipped = 0;

  for (let r = 0; r < rows.length; r += 1) {
    const value = rows[r][index];
    const text = value === null || value === undefined ? '' : display(value);
    // Anything not ISO-shaped is left out rather than guessed at. Checked by
    // position rather than by a regex: this runs once per row of the page, and a
    // page may hold every row of the result.
    if (!looksIso(text)) {
      skipped += 1;
      continue;
    }

    const [year, month, day] = [text.slice(0, 4), text.slice(0, 7), text.slice(0, 10)];
    if (!years.has(year)) years.set(year, { count: 0, months: new Map() });
    const inYear = years.get(year);
    inYear.count += 1;
    if (!inYear.months.has(month)) inYear.months.set(month, { count: 0, days: new Map() });
    const inMonth = inYear.months.get(month);
    inMonth.count += 1;
    inMonth.days.set(day, (inMonth.days.get(day) ?? 0) + 1);
  }

  const ascending = ([a], [b]) => (a < b ? -1 : 1);
  const tree = [...years.entries()].sort(ascending).map(([prefix, inYear]) => ({
    prefix,
    count: inYear.count,
    children: [...inYear.months.entries()].sort(ascending).map(([month, inMonth]) => ({
      prefix: month,
      count: inMonth.count,
      children: [...inMonth.days.entries()]
        .sort(ascending)
        .map(([day, count]) => ({ prefix: day, count, children: [] })),
    })),
  }));
  // No `truncated`: every row of the page is read, so the tree is the whole tree.
  return { tree, skipped };
}

/** `YYYY-MM-DD…`, tested a character at a time. */
function looksIso(text) {
  if (text.length < 10) return false;
  const digit = (at) => {
    const code = text.charCodeAt(at);
    return code >= 48 && code <= 57;
  };
  return (
    digit(0) && digit(1) && digit(2) && digit(3) &&
    text[4] === '-' && digit(5) && digit(6) &&
    text[7] === '-' && digit(8) && digit(9)
  );
}

/** The node in a [`dateTree`] with this prefix, or undefined. */
function nodeAt(tree, prefix) {
  let level = tree;
  let found;
  while (level?.length) {
    found = level.find((node) => prefix === node.prefix || prefix.startsWith(node.prefix));
    if (!found) return undefined;
    if (found.prefix === prefix) return found;
    level = found.children;
  }
  return undefined;
}

/** Whether `prefix` is selected, either in its own right or by an ancestor. */
export function isCovered(prefixes, prefix) {
  if (!prefixes?.size) return false;
  for (const chosen of prefixes) if (prefix === chosen || prefix.startsWith(chosen)) return true;
  return false;
}

/** Whether something *inside* `prefix` is selected but `prefix` itself is not. */
export function isPartial(prefixes, prefix) {
  if (!prefixes?.size || isCovered(prefixes, prefix)) return false;
  for (const chosen of prefixes) if (chosen.startsWith(prefix)) return true;
  return false;
}

/**
 * Everything under `ancestor` except the branch leading to `target` — what
 * unticking a month inside a ticked year has to leave behind.
 */
function siblingsAlong(tree, ancestor, target) {
  const out = [];
  let node = nodeAt(tree, ancestor);
  while (node && node.prefix !== target) {
    const onPath = node.children.find(
      (child) => target === child.prefix || target.startsWith(child.prefix),
    );
    if (!onPath) break;
    for (const child of node.children) if (child !== onPath) out.push(child.prefix);
    node = onPath;
  }
  return out;
}

/**
 * Tick or untick one node of the year/month/day tree.
 *
 * The selection is kept as the *coarsest* prefixes that describe it — ticking a
 * year stores `2022`, not its twelve months — which is what keeps the filter cheap
 * and the stored state readable.
 *
 * Unticking is the interesting half: a month inside a ticked year is not in the
 * set at all, so removing it means replacing the year with everything else the
 * year contains. Doing it any other way leaves a box that stays ticked when you
 * click it.
 */
export function toggleDatePrefix(prefixes, tree, prefix, on) {
  const next = new Set(prefixes ?? []);

  if (on) {
    // Anything this now covers is redundant.
    for (const chosen of [...next]) {
      if (chosen !== prefix && chosen.startsWith(prefix)) next.delete(chosen);
    }
    // And if an ancestor already covers it, there is nothing to add.
    if (!isCovered(next, prefix)) next.add(prefix);
    return next;
  }

  if (next.has(prefix)) {
    next.delete(prefix);
    return next;
  }

  const ancestor = [...next].find((chosen) => prefix.startsWith(chosen));
  if (!ancestor) return next;
  next.delete(ancestor);
  for (const sibling of siblingsAlong(tree, ancestor, prefix)) next.add(sibling);
  return next;
}

/**
 * One column's whole filter: the text box, a set of exact values, and a set of
 * date prefixes. Whichever parts are present all have to pass.
 *
 * They combine rather than override, because that is the only rule that stays
 * true as you use them: ticking a value never silently discards the text you
 * typed above it.
 */
export function columnPredicate(filter, logical) {
  if (!filter) return null;
  const parts = [];

  const text = predicateFor(filter.text ?? '', logical);
  if (text) parts.push(text);

  // `instanceof Set`, not `.size`: an empty set is "nothing passes", which is a
  // filter. Only its absence means "no restriction".
  if (filter.include instanceof Set) {
    const { include } = filter;
    parts.push((value) => include.has(display(value)));
  }

  if (isRange(filter.range)) {
    const { min, max } = filter.range;
    parts.push((value) => {
      // A number that is not there is outside every range, for the same reason
      // NULL satisfies no comparison.
      if (value === null || value === undefined || value === '') return false;
      const number = Number(value);
      if (Number.isNaN(number)) return false;
      if (min !== null && min !== undefined && number < min) return false;
      if (max !== null && max !== undefined && number > max) return false;
      return true;
    });
  }

  if (filter.prefixes?.size) {
    const prefixes = [...filter.prefixes];
    parts.push((value) => {
      if (value === null || value === undefined) return false;
      const shown = display(value);
      return prefixes.some((prefix) => shown.startsWith(prefix));
    });
  }

  if (parts.length === 0) return null;
  if (parts.length === 1) return parts[0];
  return (value) => parts.every((part) => part(value));
}
