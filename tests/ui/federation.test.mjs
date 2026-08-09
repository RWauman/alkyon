// Reading a DEFINE block while it is being typed. Run with:
//
//   node --test tests/ui/federation.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { declarations, importAt, projection } from '../../src/ui/federation.js';

test('every kind of declaration, with its source and database', () => {
  const found = declarations(`DEFINE
    ATTACH pg     = user:pg-dev/warehouse
    IMPORT orders = mssql-dev/sales AS ( select top 5 id from sales.order_line )
    FILES  trips  = taxi/2022/*.parquet
    EXCEL  budget = budgets/2026.xlsx#Forecast
EVALUATE
    select * from orders;`);

  assert.deepEqual(
    found.map(({ kind, alias, source, database }) => ({ kind, alias, source, database })),
    [
      { kind: 'ATTACH', alias: 'pg', source: 'user:pg-dev', database: 'warehouse' },
      { kind: 'IMPORT', alias: 'orders', source: 'mssql-dev', database: 'sales' },
      // A FILES `/` starts a glob, not a database — the opposite rule.
      { kind: 'FILES', alias: 'trips', source: 'taxi', database: null },
      { kind: 'EXCEL', alias: 'budget', source: 'budgets', database: null },
    ],
  );
});

/**
 * The columns of `select * from x` are knowable from the schema alkyon already
 * holds, which is what makes completion work for an import without running it.
 */
test('a plain select * names its table, and anything else does not', () => {
  const table = (sql) =>
    declarations(`DEFINE\n  IMPORT s = src AS ( ${sql} )\nEVALUATE\n  s`)[0].table;

  assert.equal(table('select * from customers'), 'customers');
  assert.equal(table('SELECT  *  FROM  public.customers ;'), 'public.customers');
  assert.equal(table('select * from "odd name"'), 'odd name');

  // Anything with work in it: the columns are not the table's, so do not pretend.
  assert.equal(table('select id, name from customers'), null);
  assert.equal(table('select * from customers where id = 1'), null);
  assert.equal(table('select * from a join b on a.id = b.id'), null);
});

/** Native SQL runs over as many lines as it likes; the declaration is still one. */
test('a multi-line import is one declaration', () => {
  const found = declarations(`DEFINE
    IMPORT orders = mssql-dev/sales AS (
        SELECT TOP 1000 order_id, unit_price
        FROM sales.order_line
    )
    ATTACH pg = pg-dev
EVALUATE
    select 1;`);
  assert.deepEqual(found.map((d) => d.alias), ['orders', 'pg']);
});

/**
 * The buffer is read while it is being typed, so half of everything is normal.
 * Reporting what is there beats switching completion off until the block parses.
 */
test('a half-typed block still reports what it has', () => {
  assert.deepEqual(
    declarations('DEFINE\n    ATTACH pg = pg-dev\n    IMPORT o = ').map((d) => d.alias),
    ['pg', 'o'],
  );
  // No EVALUATE yet.
  assert.equal(declarations('DEFINE\n    ATTACH pg = pg-dev\n').length, 1);
  // An unclosed bracket.
  assert.equal(
    declarations('DEFINE\n    IMPORT o = src AS ( select * from t\nEVALUATE\n o')[0].table,
    't',
  );
});

/** Nothing below EVALUATE is a declaration, whatever it looks like. */
test('the query is not scanned for declarations', () => {
  const found = declarations(`DEFINE
    ATTACH pg = pg-dev
EVALUATE
    select * from files where import = 1;`);
  assert.deepEqual(found.map((d) => d.alias), ['pg']);
});

test('an ordinary buffer declares nothing', () => {
  assert.deepEqual(declarations('select * from customers'), []);
  assert.deepEqual(declarations(''), []);
});

/**
 * Inside `AS ( … )` you are writing the *source's* SQL, so that is whose tables
 * completion has to offer — not the aliases the buffer declares, which is what the
 * query below EVALUATE completes from.
 */
test('the SQL inside an import knows which declaration it belongs to', () => {
  const buffer = `DEFINE
    IMPORT s1 = sample-parquet AS (
        SELECT c.id
        FROM public.customers c
    )
    ATTACH s2 = pg-dev/alkyon_demo
EVALUATE
    select * from s1;`;

  const at = (needle) => importAt(buffer, buffer.indexOf(needle) + 1);
  assert.equal(at('c.id').alias, 's1');
  assert.equal(at('public.customers').alias, 's1');

  // Outside the brackets is not inside them.
  assert.equal(at('ATTACH s2'), null);
  assert.equal(at('IMPORT s1'), null);
  assert.equal(at('select * from s1'), null);
});

/** A bracket is open for as long as it takes to type the query inside it. */
test('an unclosed import still claims what has been typed into it', () => {
  const buffer = 'DEFINE\n    IMPORT s1 = src AS (\n        SELECT c.\n';
  assert.equal(importAt(buffer, buffer.indexOf('SELECT c.') + 9).alias, 's1');
});

/** A bracket inside a string does not end the declaration here either. */
test('a bracket in a literal does not end the import early', () => {
  const buffer = "DEFINE\n    IMPORT s1 = src AS ( select 'a)b' as x, y\n    )\nEVALUATE\n s1";
  assert.equal(importAt(buffer, buffer.indexOf('as x')).alias, 's1');
  assert.equal(importAt(buffer, buffer.indexOf('EVALUATE')), null);
});

/**
 * The columns an import produces are written in its own select list, so they can
 * be offered without describing anything on the server.
 */
test('the select list names the columns', () => {
  assert.deepEqual(
    projection("select id, name, 'zergzrg' AS Tab, c.credit from customers c"),
    ['id', 'name', 'Tab', 'credit'],
  );
  // A bracketed argument list is one item, not three.
  assert.deepEqual(projection('select coalesce(a, b, c) as total, id from t'), [
    'total',
    'id',
  ]);
  // `from` inside a string or a function does not end the list.
  assert.deepEqual(projection("select 'from here' as note, id from t"), ['note', 'id']);
  assert.deepEqual(projection('select extract(year from d) as y, id from t'), ['y', 'id']);
  // Quoted output names lose their quotes; T-SQL brackets too.
  assert.deepEqual(projection('select 1 as "odd name", 2 as [other] from t'), [
    'odd name',
    'other',
  ]);
  // Named twice is offered once.
  assert.deepEqual(projection('select name, name from t'), ['name']);
});

/** What it cannot name, it leaves out rather than inventing. */
test('an unnameable item is skipped, not guessed', () => {
  // `*` belongs to a table this does not resolve.
  assert.deepEqual(projection('select * from t'), null);
  assert.deepEqual(projection('select c.*, id from t c'), ['id']);
  // A function call with no alias has no name until someone gives it one.
  assert.deepEqual(projection('select count(*), id from t'), ['id']);
  assert.equal(projection('select count(*) from t'), null);
  assert.equal(projection('not a select'), null);
});

/** And it reaches the declaration, which is what completion reads. */
test('a declaration carries the columns its query will produce', () => {
  const [only] = declarations(
    'DEFINE\n  IMPORT s1 = src AS ( SELECT id, name FROM public.customers WHERE id = 1 )\nEVALUATE\n s1',
  );
  assert.deepEqual(only.columns, ['id', 'name']);
  assert.equal(only.table, null, 'not a plain select *, so no table to look up');
});
