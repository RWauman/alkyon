// Column filter matching. Run with:
//
//   node --test tests/ui/filter.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  anyActive,
  applyFilters,
  columnPredicate,
  dateTree,
  distinctValues,
  isCovered,
  isPartial,
  isRange,
  isTemporal,
  numericBounds,
  parseFilter,
  predicateFor,
  toggleDatePrefix,
} from '../../src/ui/filter.js';

test('a plain word is a substring search', () => {
  assert.deepEqual(parseFilter('ada'), { operator: 'contains', operand: 'ada' });
  const match = predicateFor('ada', 'text');
  assert.equal(match('Ada Lovelace'), true, 'case does not matter');
  assert.equal(match('GRACE'), false);
});

test('an empty or half-typed filter matches nothing at all', () => {
  for (const text of ['', '   ', '>', '>=', '<']) {
    assert.equal(parseFilter(text), null, `\`${text}\` should not filter yet`);
    assert.equal(predicateFor(text, 'int'), null);
  }
});

test('the longer operator is read first', () => {
  // `>=5` must not parse as "greater than the text `=5`".
  assert.deepEqual(parseFilter('>=5'), { operator: '>=', operand: '5' });
  assert.deepEqual(parseFilter('<= 10'), { operator: '<=', operand: '10' });
  assert.deepEqual(parseFilter('>1'), { operator: '>', operand: '1' });
  // `<>` is SQL's spelling of the same thing, so it is accepted and normalised.
  assert.deepEqual(parseFilter('<>3'), { operator: '!=', operand: '3' });
});

test('a decimal column compares as a number, not as text', () => {
  // The case that makes this necessary: `decimal` arrives as a *string* so its
  // digits survive JSON, and comparing text puts 999.99 above 1000.
  const match = predicateFor('>= 1000', 'decimal');
  assert.equal(match('1000.50'), true);
  assert.equal(match('999.99'), false);

  // The same filter on a text column can only compare text, and says so by
  // behaving consistently rather than guessing.
  const text = predicateFor('>= 1000', 'text');
  assert.equal(text('999.99'), true, '"999.99" > "1000" as text');
});

test('NULL satisfies no comparison, including a negative one', () => {
  assert.equal(predicateFor('> 0', 'int')(null), false);
  // The trap: treating NULL as "not 5" turns `!=5` into "not 5, plus unknown".
  assert.equal(predicateFor('!= 5', 'int')(null), false);
  // It is still findable as text, because that is what the cell shows.
  assert.equal(predicateFor('null', 'int')(null), true);
});

test('a non-numeric cell in a numeric column drops out rather than throwing', () => {
  const match = predicateFor('> 5', 'int');
  assert.equal(match('<decode error: nope>'), false);
});

const META = [{ logical: 'text' }, { logical: 'int' }, { logical: 'decimal' }];
const ROWS = [
  ['ada', 1, '1000.50'],
  ['grace', 2, '2000.25'],
  ['alan', 3, null],
  ['ADA', 4, '10.00'],
];

test('no active filter means no index array to build', () => {
  assert.equal(applyFilters(ROWS, {}, META), null);
  assert.equal(applyFilters(ROWS, { 0: { text: '  ' } }, META), null);
  assert.equal(anyActive({ 0: { text: '' }, 1: { text: '>' } }), false);
  assert.equal(anyActive({ 0: { text: 'x' } }), true);
  assert.equal(anyActive({ 0: { include: new Set(['ada']) } }), true);
  assert.equal(anyActive({ 0: { prefixes: new Set(['2022']) } }), true);
});

test('an empty set of ticked values means nothing passes, not everything', () => {
  // The bug this guards. `null` is "no restriction"; an empty set is "none of
  // them". Conflating the two made unticking the last value show every row again
  // — and left every box ticked, so ticking one more did nothing at all.
  assert.equal(anyActive({ 0: { include: new Set() } }), true);
  assert.deepEqual(applyFilters(ROWS, { 0: { include: new Set() } }, META), []);
  assert.equal(applyFilters(ROWS, { 0: { include: null } }, META), null);

  // And ticking one back on filters to that one rather than resetting.
  assert.deepEqual(applyFilters(ROWS, { 0: { include: new Set(['grace']) } }, META), [1]);
});

test('filters combine, and the result keeps the original order', () => {
  assert.deepEqual(applyFilters(ROWS, { 0: { text: 'ada' } }, META), [0, 3]);
  // Two columns at once: ada rows with id above 1.
  assert.deepEqual(applyFilters(ROWS, { 0: { text: 'ada' }, 1: { text: '> 1' } }, META), [3]);
  // Indices point back into the unfiltered rows, which is what lets a sort and a
  // filter compose without either one copying the data.
  assert.deepEqual(applyFilters(ROWS, { 2: { text: '>= 1000' } }, META), [0, 1]);
});

test('a filter that matches nothing returns an empty list, not null', () => {
  // The distinction matters: null means "unfiltered", so returning it here would
  // show every row when the answer is none.
  assert.deepEqual(applyFilters(ROWS, { 0: { text: 'zzz' } }, META), []);
});

test('ticked values match exactly, not as substrings', () => {
  const match = columnPredicate({ include: new Set(['ada']) }, 'text');
  assert.equal(match('ada'), true);
  assert.equal(match('ADA'), false, 'a ticked value is the value, not a search');
  assert.equal(match('ada lovelace'), false);
  // NULL is listed as the text the cell shows, so it can be ticked like any other.
  assert.equal(columnPredicate({ include: new Set(['NULL']) }, 'int')(null), true);
});

test('the text box and the ticked values both have to pass', () => {
  // Ticking a value must not silently discard what was typed above it.
  const both = columnPredicate({ text: 'a', include: new Set(['ada', 'grace']) }, 'text');
  assert.equal(both('ada'), true);
  assert.equal(both('grace'), true, '"grace" contains an a');
  const narrower = columnPredicate({ text: 'ad', include: new Set(['ada', 'grace']) }, 'text');
  assert.equal(narrower('grace'), false);
});

const DATES = [
  ['2022-01-05'],
  ['2022-01-31'],
  ['2022-03-15'],
  ['2023-07-01T10:30:00'],
  [null],
  ['not a date'],
];

test('a date prefix selects a year, a month or a day', () => {
  const year = columnPredicate({ prefixes: new Set(['2022']) }, 'date');
  assert.equal(year('2022-03-15'), true);
  assert.equal(year('2023-07-01'), false);

  const month = columnPredicate({ prefixes: new Set(['2022-01']) }, 'date');
  assert.equal(month('2022-01-31'), true);
  assert.equal(month('2022-03-15'), false);

  // A timestamp is the same string with more on the end, so the same prefix works.
  assert.equal(columnPredicate({ prefixes: new Set(['2023-07']) }, 'timestamp')('2023-07-01T10:30:00'), true);
  // NULL is not in any month.
  assert.equal(year(null), false);
});

test('several prefixes are a union', () => {
  const match = columnPredicate({ prefixes: new Set(['2022-01', '2023']) }, 'date');
  assert.equal(match('2022-01-05'), true);
  assert.equal(match('2022-03-15'), false);
  assert.equal(match('2023-07-01T10:30:00'), true);
});

test('the date tree is built from the text, with no date parsing', () => {
  const { tree, skipped } = dateTree(DATES, 0);
  assert.equal(skipped, 2, 'NULL and the unparseable value are left out, not guessed at');

  assert.deepEqual(tree.map((y) => [y.prefix, y.count]), [['2022', 3], ['2023', 1]]);
  assert.deepEqual(
    tree[0].children.map((m) => [m.prefix, m.count]),
    [['2022-01', 2], ['2022-03', 1]],
  );
  assert.deepEqual(
    tree[0].children[0].children.map((d) => [d.prefix, d.count]),
    [['2022-01-05', 1], ['2022-01-31', 1]],
  );
  // Years, months and days all come back in calendar order.
  assert.equal(tree[1].children[0].prefix, '2023-07');
});

test('only date-like columns get the year/month/day picker', () => {
  assert.equal(isTemporal('date'), true);
  assert.equal(isTemporal('timestamp'), true);
  assert.equal(isTemporal('timestamp_tz'), true);
  assert.equal(isTemporal('text'), false);
  assert.equal(isTemporal('int'), false);
});

test('ticking a year stores the year, not its twelve months', () => {
  const { tree } = dateTree(DATES, 0);
  const chosen = toggleDatePrefix(new Set(), tree, '2022', true);
  assert.deepEqual([...chosen], ['2022']);

  // Ticking a month inside it adds nothing: the year already covers it.
  assert.deepEqual([...toggleDatePrefix(chosen, tree, '2022-01', true)], ['2022']);
  // And ticking the year over a month replaces it rather than keeping both.
  const fromMonth = toggleDatePrefix(new Set(['2022-01', '2022-03']), tree, '2022', true);
  assert.deepEqual([...fromMonth], ['2022']);
});

test('unticking a month inside a ticked year leaves the rest of the year', () => {
  const { tree } = dateTree(DATES, 0);
  // This is the case that a naive implementation gets wrong: `2022-01` is not in
  // the set at all, so "remove it" has to mean "expand 2022 into everything else".
  const after = toggleDatePrefix(new Set(['2022']), tree, '2022-01', false);
  assert.deepEqual([...after].sort(), ['2022-03']);

  const match = columnPredicate({ prefixes: after }, 'date');
  assert.equal(match('2022-01-05'), false);
  assert.equal(match('2022-03-15'), true);
});

test('a box reads ticked when an ancestor is ticked, and partial when a child is', () => {
  const chosen = new Set(['2022-01']);
  assert.equal(isCovered(chosen, '2022-01'), true);
  assert.equal(isCovered(chosen, '2022-01-05'), true, 'a day inside a ticked month');
  assert.equal(isCovered(chosen, '2022'), false);
  assert.equal(isPartial(chosen, '2022'), true, 'the year holds a ticked month');
  assert.equal(isPartial(chosen, '2022-01'), false, 'ticked is not partial');
  assert.equal(isPartial(new Set(), '2022'), false);
});

test('unticking what was ticked directly just removes it', () => {
  const { tree } = dateTree(DATES, 0);
  assert.deepEqual([...toggleDatePrefix(new Set(['2022', '2023']), tree, '2022', false)], ['2023']);
  // Unticking something nothing covers is a no-op rather than an error.
  assert.deepEqual([...toggleDatePrefix(new Set(['2023']), tree, '2022-01', false)], ['2023']);
});

test('distinct values come back most frequent first, with counts', () => {
  const rows = [['be'], ['fr'], ['be'], [null], ['be'], ['fr']];
  const { values, truncated } = distinctValues(rows, 0);
  assert.deepEqual(values, [
    { value: 'be', count: 3 },
    { value: 'fr', count: 2 },
    { value: 'NULL', count: 1 },
  ]);
  assert.equal(truncated, false);
});

test('every row of the page is counted, with no sampling', () => {
  // The counts describe the page, so they have to be the page's counts. Sampling
  // would make them quietly wrong and the set quietly incomplete.
  const rows = Array.from({ length: 250_000 }, (_, i) => [i % 3 === 0 ? 'be' : 'fr']);
  const { values, truncated } = distinctValues(rows, 0);
  assert.equal(truncated, false);
  assert.equal(values.reduce((total, one) => total + one.count, 0), 250_000);
});

test('too many distinct values stops the scan rather than finishing it', () => {
  const rows = Array.from({ length: 50 }, (_, i) => [`v${i}`]);
  const { values, truncated } = distinctValues(rows, 0, { max: 10 });
  assert.equal(truncated, true, 'a partial list must not read as the whole set');
  assert.deepEqual(values, [], 'and no partial list is handed back at all');
});

test('numeric bounds ignore what is not a number', () => {
  const rows = [[5], ['12.5'], [null], ['not a number'], [-3], ['']];
  assert.deepEqual(numericBounds(rows, 0), { min: -3, max: 12.5, counted: 3, missing: 3 });
  // A column with nothing numeric in it has no bounds rather than 0..0.
  assert.deepEqual(numericBounds([[null], ['x']], 0), {
    min: null, max: null, counted: 0, missing: 2,
  });
});

test('a range keeps what falls inside it, both ends optional', () => {
  const between = columnPredicate({ range: { min: 10, max: 20 } }, 'int');
  assert.equal(between(9), false);
  assert.equal(between(10), true, 'the bounds are inclusive');
  assert.equal(between(20), true);
  assert.equal(between(21), false);

  assert.equal(columnPredicate({ range: { min: 10, max: null } }, 'int')(1000), true);
  assert.equal(columnPredicate({ range: { min: null, max: 10 } }, 'int')(1000), false);

  // A decimal arrives as text and still compares as a number.
  assert.equal(columnPredicate({ range: { min: 1000, max: null } }, 'decimal')('1000.50'), true);
  assert.equal(columnPredicate({ range: { min: 1000, max: null } }, 'decimal')('999.99'), false);

  // Nothing numeric is inside any range, NULL included.
  for (const absent of [null, undefined, '', 'wat']) {
    assert.equal(between(absent), false, `${absent} should be outside`);
  }
});

test('a range with both ends open is not a filter', () => {
  assert.equal(isRange(null), false);
  assert.equal(isRange({ min: null, max: null }), false);
  assert.equal(isRange({ min: 0, max: null }), true, '0 is a bound, not an absence');
  assert.equal(anyActive({ 0: { range: { min: null, max: null } } }), false);
  assert.equal(anyActive({ 0: { range: { min: 5, max: null } } }), true);
  assert.equal(columnPredicate({ range: { min: null, max: null } }, 'int'), null);
});

test('None over a narrowed list clears only what is shown', () => {
  // The button is back, and with the empty-set semantics fixed it now does what it
  // says. This is the composition rule: it acts on the *shown* values.
  const all = new Set(['ada', 'grace', 'alan']);
  const shown = ['ada'];
  const after = new Set(all);
  for (const one of shown) after.delete(one);
  assert.deepEqual([...after].sort(), ['alan', 'grace']);
  assert.equal(anyActive({ 0: { include: after } }), true);
});
