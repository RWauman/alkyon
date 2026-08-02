// The TARGET directive's dialect-safety rules. Run with:
//
//   node --test tests/ui/target.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { parseTarget, targetAt } from '../../src/ui/target.js';

const sources = ['pg-dev', 'mssql-dev', 'my.source', 'user:warehouse', 'project:warehouse'];
const known = (id) => sources.includes(id);
const parse = (sql) => parseTarget(sql, known);

test('retargets the source', () => {
  assert.deepEqual(parse('TARGET pg-dev'), { targets: [{ id: 'pg-dev' }], sql: '' });
});

test('retargets source and database', () => {
  assert.deepEqual(parse('TARGET pg-dev.warehouse'), {
    targets: [{ id: 'pg-dev', database: 'warehouse' }],
    sql: '',
  });
});

test('the keyword is case-insensitive', () => {
  assert.deepEqual(parse('target pg-dev').targets, [{ id: 'pg-dev' }]);
  assert.deepEqual(parse('  TaRgEt   pg-dev  ').targets, [{ id: 'pg-dev' }]);
});

/**
 * The directive used to be spelled `SWAP`, which said "exchange" rather than
 * "point at". Files saved before the rename keep working.
 */
test('SWAP, the former spelling, is still accepted', () => {
  assert.deepEqual(parse('SWAP pg-dev'), { targets: [{ id: 'pg-dev' }], sql: '' });
  assert.deepEqual(parse('swap pg-dev.warehouse').targets, [
    { id: 'pg-dev', database: 'warehouse' },
  ]);
  assert.deepEqual(parse('SWAP pg-dev;\nTARGET mssql-dev.master;\nselect 1').targets, [
    { id: 'pg-dev' },
    { id: 'mssql-dev', database: 'master' },
  ]);
  assert.deepEqual(targetAt(['SWAP pg'], 0, 7), { token: 'pg', start: 5, end: 7 });
});

test('carries on with the rest of the buffer', () => {
  for (const sql of [
    'TARGET pg-dev; select 1',
    'TARGET pg-dev;\nselect 1',
    'TARGET pg-dev\nselect 1',
  ]) {
    const parsed = parse(sql);
    assert.deepEqual(parsed.targets, [{ id: 'pg-dev' }], sql);
    assert.equal(parsed.sql, 'select 1', sql);
  }
});

/**
 * The bug this guards is embarrassing and was live for months: the editor opens
 * on two comment lines, the second of which explains the directive — and because
 * the directive had to be the very first thing in the buffer, using it exactly as
 * documented did nothing at all. Any `.sql` file with a header comment was in
 * the same position.
 */
test('a comment above a directive does not disable it', () => {
  const header = [
    '-- Ctrl+Enter to run. With text selected, only the selection runs.',
    '-- TARGET <source> or TARGET <source>.<database> retargets the editor.',
  ].join('\n');

  const parsed = parse(`${header}\nTARGET pg-dev\nselect 1;`);
  assert.deepEqual(parsed.targets, [{ id: 'pg-dev' }]);
  // The comments are the user's and stay put; only the directive is consumed.
  assert.equal(parsed.sql, `${header}\nselect 1;`);
});

test('comments between directives are allowed too', () => {
  const parsed = parse('-- one\nTARGET pg-dev\n\n-- two\nTARGET mssql-dev\nselect 1;');
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
  assert.deepEqual(targetAt(['-- a header', 'TARGET pg'], 1, 9), {
    token: 'pg',
    start: 7,
    end: 9,
  });
});

test('applies several directives in order', () => {
  const parsed = parse('TARGET pg-dev;\nTARGET mssql-dev.master;\nselect 1');
  assert.deepEqual(parsed.targets, [
    { id: 'pg-dev' },
    { id: 'mssql-dev', database: 'master' },
  ]);
  assert.equal(parsed.sql, 'select 1');
});

test('an id containing dots wins over a database split', () => {
  assert.deepEqual(parse('TARGET my.source').targets, [{ id: 'my.source' }]);
  assert.deepEqual(parse('TARGET my.source.reporting').targets, [
    { id: 'my.source', database: 'reporting' },
  ]);
});

test('a scope-qualified key is a valid target', () => {
  assert.deepEqual(parse('TARGET project:warehouse').targets, [{ id: 'project:warehouse' }]);
  assert.deepEqual(parse('TARGET user:warehouse.staging').targets, [
    { id: 'user:warehouse', database: 'staging' },
  ]);
  assert.equal(parse('TARGET project:warehouse; select 1').sql, 'select 1');
});

test('an unknown target is reported, not guessed at', () => {
  assert.deepEqual(parse('TARGET nope'), { unknown: 'nope' });
  assert.deepEqual(parse('TARGET pg-dev.db; TARGET nope'), { unknown: 'nope' });
});

// The reason the directive is TARGET and not USE or SOURCE. Each of these must
// reach the engine untouched.
test('real SQL is never mistaken for a directive', () => {
  const untouched = [
    // T-SQL's MERGE names a target, but always mid-statement.
    'MERGE INTO a USING b ON a.id = b.id WHEN NOT MATCHED BY TARGET THEN INSERT VALUES (b.id)',
    // A column, alias or table that merely contains the word.
    'select target from t',
    'select 1 as target',
    'select * from target',
    // T-SQL, MySQL, DuckDB and ClickHouse all have a real USE statement.
    'USE warehouse',
    'USE warehouse; SELECT 1;',
    // Snowflake: the only dialect that uses SWAP, always mid-statement.
    'ALTER TABLE a SWAP WITH b',
    'alter table sales.orders swap with sales.orders_new;',
    // MySQL/MariaDB have SWAPS as a keyword — a different token.
    'SWAPS foo',
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
 * instead of SQL keywords. It used to offer keywords: `TARGET pg` was answered
 * with `PG_CONTEXT`, and since the list pops up as you type, Enter accepted it
 * and the directive quietly stopped being one.
 */
test('the target is recognised while it is being typed', () => {
  assert.deepEqual(targetAt(['TARGET pg'], 0, 9), { token: 'pg', start: 7, end: 9 });
  assert.deepEqual(targetAt(['TARGET '], 0, 7), { token: '', start: 7, end: 7 });
  // Halfway through a name: the whole target is replaced, tail included.
  assert.deepEqual(targetAt(['TARGET pg-dev'], 0, 9), { token: 'pg', start: 7, end: 13 });
  // Lower case, and leading whitespace.
  assert.deepEqual(targetAt(['  target x'], 0, 10), { token: 'x', start: 9, end: 10 });
});

test('a second directive is still a directive', () => {
  assert.deepEqual(targetAt(['TARGET pg-dev', 'TARGET my'], 1, 9), {
    token: 'my',
    start: 7,
    end: 9,
  });
});

test('anything that is not a leading directive is ordinary SQL', () => {
  // Past the end of the target — you have moved on.
  assert.equal(targetAt(['TARGET pg-dev '], 0, 14), null);
  // Before the keyword.
  assert.equal(targetAt(['TARGET pg'], 0, 2), null);
  // Not a directive at all.
  assert.equal(targetAt(['select * from t'], 0, 15), null);
  // A directive buried under real SQL is not one, so its "target" must not
  // complete.
  assert.equal(targetAt(['select 1;', 'TARGET pg'], 1, 9), null);
});
