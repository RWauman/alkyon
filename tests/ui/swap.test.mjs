// The SWAP directive's dialect-safety rules. Run with:
//
//   node --test tests/ui/swap.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { parseSwap } from '../../src/ui/swap.js';

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
