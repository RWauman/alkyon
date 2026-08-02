// Reading `source.database.table` out of ordinary SQL. Run with:
//
//   node --test tests/ui/qualified.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { findCrossSource, unresolvedHeads } from '../../src/ui/qualified.js';

// `taxi_data` is a folder source: one catalogue, no databases.
const servers = ['sales_db', 'pg-dev', 'my.source', 'user:warehouse'];
const folders = ['taxi_data', 'taxi-data'];
const find = (sql) =>
  findCrossSource(sql, (id) =>
    servers.includes(id) ? { database: true } : folders.includes(id) ? { database: false } : null,
  );

test('a three-part name names the source and the database', () => {
  assert.deepEqual(find('select * from sales_db.warehouse.customer'), {
    source: 'sales_db',
    database: 'warehouse',
    sql: 'select * from customer',
  });
});

test('everything after the database is left exactly as written', () => {
  // The schema is the engine's business. Alkyon takes its two parts and stops.
  assert.deepEqual(find('select * from sales_db.warehouse.sales.customer'), {
    source: 'sales_db',
    database: 'warehouse',
    sql: 'select * from sales.customer',
  });
  // Quoting included — it is copied out of the original text, not rebuilt.
  assert.equal(
    find('select * from sales_db.warehouse."2022"."yellow"').sql,
    'select * from "2022"."yellow"',
  );
});

test('an id that is not a bare identifier can be quoted', () => {
  assert.deepEqual(find('select * from "pg-dev".alkyon_demo.sales.customer'), {
    source: 'pg-dev',
    database: 'alkyon_demo',
    sql: 'select * from sales.customer',
  });
});

test('a source id containing dots wins over a shorter match', () => {
  assert.deepEqual(find('select * from my.source.reporting.t'), {
    source: 'my.source',
    database: 'reporting',
    sql: 'select * from t',
  });
});

test('ordinary SQL is left alone, and costs nothing', () => {
  for (const sql of [
    'select * from customer',
    'select * from sales.customer',
    // Three parts, but the first names no source: legal T-SQL, untouched.
    'select * from alkyon_demo.sales.customer',
    'select a.b.c from t',
  ]) {
    assert.equal(find(sql), null, sql);
  }
});

test('two parts are never enough', () => {
  // `sales_db.warehouse` has no table in it, so there is nothing to retarget to.
  assert.equal(find('select * from sales_db.warehouse'), null);
});

test('every reference must agree on one target', () => {
  const both = find(
    'select * from sales_db.warehouse.a join sales_db.other.b on true',
  );
  assert.deepEqual(both, { conflict: ['sales_db/warehouse', 'sales_db/other'] });
});

test('the same target twice is fine, and both are rewritten', () => {
  assert.deepEqual(
    find('select * from sales_db.warehouse.a join sales_db.warehouse.b on a.id = b.id'),
    {
      source: 'sales_db',
      database: 'warehouse',
      sql: 'select * from a join b on a.id = b.id',
    },
  );
});

test('a name inside a string or a comment is not a name', () => {
  for (const sql of [
    "select 'sales_db.warehouse.customer' as note",
    '-- select * from sales_db.warehouse.customer',
    '/* sales_db.warehouse.customer */ select 1',
    "select 'it''s sales_db.warehouse.x' from t",
  ]) {
    assert.equal(find(sql), null, sql);
  }
});

test('a reference after a comment is still found', () => {
  assert.equal(
    find('-- a header\nselect * from sales_db.warehouse.customer').sql,
    '-- a header\nselect * from customer',
  );
});

/**
 * A folder source has no databases — DuckDB gives it one catalogue and that is
 * that — so there is no database part to take, and the subdirectory comes
 * straight after the source. Taking one anyway sent `"2022"` as the database and
 * the query went nowhere.
 */
test('a folder source has no database part', () => {
  assert.deepEqual(find('select * from taxi_data."2022".yellow_202212'), {
    source: 'taxi_data',
    database: null,
    sql: 'select * from "2022".yellow_202212',
  });
  // Quoted, because a dash is not a bare SQL identifier.
  assert.deepEqual(find('select * from "taxi-data"."2022".yellow'), {
    source: 'taxi-data',
    database: null,
    sql: 'select * from "2022".yellow',
  });
  // At the root of the folder, two parts is the whole name.
  assert.deepEqual(find('select * from taxi_data.zones'), {
    source: 'taxi_data',
    database: null,
    sql: 'select * from zones',
  });
});

test('a folder source and a server source still conflict', () => {
  assert.deepEqual(find('select * from taxi_data.zones z join sales_db.warehouse.t t on true'), {
    conflict: ['taxi_data', 'sales_db/warehouse'],
  });
});

/**
 * A name that *nearly* matched is worth pointing at once the engine has refused.
 * `taxi_data.…` when the source is `taxi-data` is not retargeted at all — it goes
 * to whatever the editor was on and fails there, about a table nobody asked
 * about. This is what turns that into a pointer.
 */
test('the heads that resolved to nothing are reported', () => {
  // Only the dashed spelling exists, which is exactly the situation that
  // produced the confusing error.
  const known = (id) => id === 'taxi-data';
  assert.deepEqual(
    unresolvedHeads('select * from taxi_data."2022".yellow_202212', known),
    ['taxi_data'],
  );
  // A head that did resolve is not a near miss.
  assert.deepEqual(
    unresolvedHeads('select * from taxi_data.a join "taxi-data".b on true', known),
    ['taxi_data'],
  );
  // Ordinary two-part SQL has a head too, and it is reported — the caller only
  // acts on the ones that look like a source id it knows.
  assert.deepEqual(unresolvedHeads('select * from sales.customer', known), ['sales']);
  // Nothing dotted, nothing to report.
  assert.deepEqual(unresolvedHeads('select * from customer', known), []);
  assert.deepEqual(unresolvedHeads("select 'a.b.c' from t", known), []);
});

test('an alias that merely starts like a source is untouched', () => {
  // `sales_dbx` is not `sales_db`, and a chain may not start mid-word.
  assert.equal(find('select * from sales_dbx.warehouse.customer'), null);
  assert.equal(find('select * from x.sales_db.warehouse.customer'), null);
});
