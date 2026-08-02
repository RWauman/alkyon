// The SWAP directive's dialect-safety rules. Run with:
//
//   node --test tests/ui/swap.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { parseSwap, swapTargetAt } from '../../src/ui/swap.js';

const sources = ['pg-dev', 'mssql-dev', 'my.source', 'user:warehouse', 'project:warehouse'];
const known = (id) => sources.includes(id);
const parse = (sql) => parseSwap(sql, known);

test('swaps the source', () => {
  assert.deepEqual(parse('SWAP pg-dev'), { targets: [{ id: 'pg-dev' }], sql: '' });
});

test('swaps source and database', () => {
  assert.deepEqual(parse('SWAP pg-dev.warehouse'), {
    targets: [{ id: 'pg-dev', database: 'warehouse' }],
    sql: '',
  });
});

test('the keyword is case-insensitive', () => {
  assert.deepEqual(parse('swap pg-dev').targets, [{ id: 'pg-dev' }]);
  assert.deepEqual(parse('  SwAp   pg-dev  ').targets, [{ id: 'pg-dev' }]);
});

test('carries on with the rest of the buffer', () => {
  for (const sql of [
    'SWAP pg-dev; select 1',
    'SWAP pg-dev;\nselect 1',
    'SWAP pg-dev\nselect 1',
  ]) {
    const parsed = parse(sql);
    assert.deepEqual(parsed.targets, [{ id: 'pg-dev' }], sql);
    assert.equal(parsed.sql, 'select 1', sql);
  }
});

/**
 * The bug this guards is embarrassing and was live for months: the editor opens
 * on two comment lines, the second of which explains SWAP — and because the
 * directive had to be the very first thing in the buffer, using it exactly as
 * documented did nothing at all. Any `.sql` file with a header comment was in
 * the same position.
 */
test('a comment above a directive does not disable it', () => {
  const header = [
    '-- Ctrl+Enter to run. With text selected, only the selection runs.',
    '-- SWAP <source> or SWAP <source>.<database> retargets the editor.',
  ].join('\n');

  const parsed = parse(`${header}\nSWAP pg-dev\nselect 1;`);
  assert.deepEqual(parsed.targets, [{ id: 'pg-dev' }]);
  // The comments are the user's and stay put; only the directive is consumed.
  assert.equal(parsed.sql, `${header}\nselect 1;`);
});

test('comments between directives are allowed too', () => {
  const parsed = parse('-- one\nSWAP pg-dev\n\n-- two\nSWAP mssql-dev\nselect 1;');
  assert.deepEqual(parsed.targets, [{ id: 'pg-dev' }, { id: 'mssql-dev' }]);
  // Both comments survive. The blank line that followed a directive does not —
  // the directive's own trailing `\s*` takes it, which is whitespace between
  // statements and changes nothing but the line count.
  assert.equal(parsed.sql, '-- one\n-- two\nselect 1;');
});

test('a buffer of nothing but comments is left exactly as it is', () => {
  const only = '-- just a note\nselect 1;';
  assert.deepEqual(parse(only), { targets: [], sql: only });
});

test('the completion sees past a header comment as well', () => {
  assert.deepEqual(swapTargetAt(['-- a header', 'SWAP pg'], 1, 7), {
    token: 'pg',
    start: 5,
    end: 7,
  });
});

test('applies several directives in order', () => {
  const parsed = parse('SWAP pg-dev;\nSWAP mssql-dev.master;\nselect 1');
  assert.deepEqual(parsed.targets, [
    { id: 'pg-dev' },
    { id: 'mssql-dev', database: 'master' },
  ]);
  assert.equal(parsed.sql, 'select 1');
});

test('an id containing dots wins over a database split', () => {
  assert.deepEqual(parse('SWAP my.source').targets, [{ id: 'my.source' }]);
  assert.deepEqual(parse('SWAP my.source.reporting').targets, [
    { id: 'my.source', database: 'reporting' },
  ]);
});

test('a scope-qualified key is a valid target', () => {
  assert.deepEqual(parse('SWAP project:warehouse').targets, [{ id: 'project:warehouse' }]);
  assert.deepEqual(parse('SWAP user:warehouse.staging').targets, [
    { id: 'user:warehouse', database: 'staging' },
  ]);
  assert.equal(parse('SWAP project:warehouse; select 1').sql, 'select 1');
});

test('an unknown target is reported, not guessed at', () => {
  assert.deepEqual(parse('SWAP nope'), { unknown: 'nope' });
  assert.deepEqual(parse('SWAP pg-dev.db; SWAP nope'), { unknown: 'nope' });
});

// The reason SWAP was chosen over USE. Each of these must reach the engine
// untouched.
test('real SQL is never mistaken for a directive', () => {
  const untouched = [
    // Snowflake: the only dialect that uses the word, always mid-statement.
    'ALTER TABLE a SWAP WITH b',
    'alter table sales.orders swap with sales.orders_new;',
    // MySQL/MariaDB have SWAPS as a keyword — a different token.
    'SWAPS foo',
    // T-SQL, MySQL, DuckDB and ClickHouse all have a real USE statement.
    'USE warehouse',
    'USE warehouse; SELECT 1;',
    // A column or alias that merely contains the word.
    'select swap from t',
    'select 1 as swap',
    // Ordinary queries.
    'select 1',
    '',
  ];

  for (const sql of untouched) {
    const parsed = parse(sql);
    assert.deepEqual(parsed.targets, [], sql);
    assert.equal(parsed.sql, sql.trim(), sql);
  }
});

// --------------------------------------------------------------- completion

/**
 * Knowing where the target sits is what lets the editor offer sources there
 * instead of SQL keywords. It used to offer keywords: `SWAP pg` was answered
 * with `PG_CONTEXT`, and since the list pops up as you type, Enter accepted it
 * and the directive quietly stopped being one.
 */
test('the SWAP target is recognised while it is being typed', () => {
  assert.deepEqual(swapTargetAt(['SWAP pg'], 0, 7), { token: 'pg', start: 5, end: 7 });
  assert.deepEqual(swapTargetAt(['SWAP '], 0, 5), { token: '', start: 5, end: 5 });
  // Halfway through a name: the whole target is replaced, tail included.
  assert.deepEqual(swapTargetAt(['SWAP pg-dev'], 0, 7), { token: 'pg', start: 5, end: 11 });
  // Lower case, and leading whitespace.
  assert.deepEqual(swapTargetAt(['  swap x'], 0, 8), { token: 'x', start: 7, end: 8 });
});

test('a second directive is still a directive', () => {
  assert.deepEqual(swapTargetAt(['SWAP pg-dev', 'SWAP my'], 1, 7), {
    token: 'my',
    start: 5,
    end: 7,
  });
});

test('anything that is not a leading directive is ordinary SQL', () => {
  // Past the end of the target — you have moved on.
  assert.equal(swapTargetAt(['SWAP pg-dev '], 0, 12), null);
  // Before the keyword.
  assert.equal(swapTargetAt(['SWAP pg'], 0, 2), null);
  // Not a directive at all.
  assert.equal(swapTargetAt(['select * from t'], 0, 15), null);
  // A SWAP buried under real SQL is not one, so its "target" must not complete.
  assert.equal(swapTargetAt(['select 1;', 'SWAP pg'], 1, 7), null);
});
