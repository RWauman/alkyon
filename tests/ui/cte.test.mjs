// Reading CTE names and columns out of a buffer. Run with:
//
//   node --test tests/ui/cte.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  cteColumns,
  mask,
  outputName,
  parseCtes,
  referencedTables,
  selectShape,
} from '../../src/ui/cte.js';

test('comments and literals are blanked without moving anything else', () => {
  const sql = "select 'a,b' as x, -- , not a comma\n  1";
  const masked = mask(sql);
  assert.equal(masked.length, sql.length, 'offsets have to keep lining up');
  // The commas inside the literal and the comment are gone; the real one is not.
  assert.equal((masked.match(/,/g) ?? []).length, 1);
  // Newlines survive, so line numbers are unchanged.
  assert.equal(masked.split('\n').length, 2);
  // A doubled quote does not end the literal early.
  assert.ok(!mask("select 'it''s, here' as x, y").slice(0, 22).includes(','));
});

test('an item names itself, or its alias', () => {
  assert.equal(outputName('id'), 'id');
  assert.equal(outputName('c.name'), 'name');
  assert.equal(outputName('sum(total) as spent'), 'spent');
  assert.equal(outputName('sum(total) spent'), 'spent');
  assert.equal(outputName('"odd name"'), 'odd name');
  assert.equal(outputName('count(*) as "n rows"'), 'n rows');
  // Nothing to offer rather than a guess.
  assert.equal(outputName('count(*)'), null);
  assert.equal(outputName('a + b'), null);
  assert.equal(outputName('*'), null);
  // A comma inside a function call must not have split the item in the first
  // place, and `coalesce(a, b)` alone has no name.
  assert.equal(outputName('coalesce(a, b)'), null);
  assert.equal(outputName('coalesce(a, b) as first'), 'first');
});

test('the select list is split on its own commas only', () => {
  const shape = selectShape("select id, coalesce(a, b) as ab, 'x,y' as lit from t");
  assert.deepEqual(shape.columns, ['id', 'ab', 'lit']);
  assert.equal(shape.from, 't');
  assert.equal(shape.star, false);
});

test('distinct and top do not become column names', () => {
  assert.deepEqual(selectShape('select distinct a, b from t').columns, ['a', 'b']);
  assert.deepEqual(selectShape('select top 100 a from t').columns, ['a']);
  assert.deepEqual(selectShape('select distinct on (a) a, b from t').columns, ['a', 'b']);
});

test('one CTE, its name and its columns', () => {
  const ctes = parseCtes('with recent as (select id, name from sales.customer) select * from recent');
  assert.deepEqual(ctes, [
    { name: 'recent', columns: ['id', 'name'], from: 'sales.customer', star: false },
  ]);
});

test('several CTEs, including one reading the one before it', () => {
  const sql = `
    with a as (select id, total from orders),
         b as (select id, sum(total) as spent from a group by id)
    select * from b`;
  assert.deepEqual(
    parseCtes(sql).map((c) => [c.name, c.columns]),
    [['a', ['id', 'total']], ['b', ['id', 'spent']]],
  );
});

test('an explicit column list is the exact answer', () => {
  const ctes = parseCtes('with t (x, y) as (select 1, 2) select * from t');
  assert.deepEqual(ctes[0].columns, ['x', 'y']);
});

test('MATERIALIZED and RECURSIVE do not confuse it', () => {
  assert.deepEqual(
    parseCtes('with recursive walk as (select n from seed) select * from walk')[0].columns,
    ['n'],
  );
  assert.deepEqual(
    parseCtes('with t as materialized (select a from x) select * from t')[0].columns,
    ['a'],
  );
  assert.deepEqual(
    parseCtes('with t as not materialized (select a from x) select * from t')[0].columns,
    ['a'],
  );
});

test('nested parentheses in the body do not end it early', () => {
  const sql = 'with t as (select id from (select id from inner_t) z) select * from t';
  const ctes = parseCtes(sql);
  assert.equal(ctes.length, 1);
  assert.deepEqual(ctes[0].columns, ['id']);
});

test('a WITH inside a subquery is not in scope', () => {
  // Its names cannot be referenced from the outer query, so offering them would
  // be offering something that does not exist.
  const sql = 'select * from (with inner_cte as (select a from t) select * from inner_cte) z';
  assert.deepEqual(parseCtes(sql), []);
});

test('`select *` resolves against a schema that is already known', () => {
  const known = { 'sales.customer': ['id', 'name', 'credit'] };
  const sql = 'with c as (select * from sales.customer) select * from c';
  assert.deepEqual(cteColumns(sql, known), { c: ['id', 'name', 'credit'] });

  // And through a chain, because the earlier CTE is in the map by then.
  const chained = 'with a as (select * from sales.customer), b as (select * from a) select 1';
  assert.deepEqual(cteColumns(chained, known).b, ['id', 'name', 'credit']);
});

test('`select *, extra` keeps both, in the order written', () => {
  const known = { t: ['a', 'b'] };
  const sql = 'with c as (select *, 1 as extra from t) select 1';
  assert.deepEqual(cteColumns(sql, known).c, ['a', 'b', 'extra']);
});

test('an unresolvable star yields no columns rather than wrong ones', () => {
  const sql = 'with c as (select * from somewhere_unknown) select 1';
  assert.deepEqual(cteColumns(sql, {}), { c: [] });
});

test('the tables a statement reads from, with their aliases', () => {
  const { names, aliases } = referencedTables(
    'select * from sales.customer c join sales.order_line as o on o.customer_id = c.id',
  );
  assert.deepEqual([...names].sort(), ['sales.customer', 'sales.order_line']);
  assert.equal(aliases.get('c'), 'sales.customer');
  assert.equal(aliases.get('o'), 'sales.order_line');
});

test('a comma-separated list, and a keyword is not an alias', () => {
  const { names, aliases } = referencedTables('select * from a, b, c where a.x = 1');
  assert.deepEqual([...names].sort(), ['a', 'b', 'c']);
  // `where` follows `c` but is not its alias.
  assert.equal(aliases.has('where'), false);
});

test('only the statement the cursor is in', () => {
  const sql = 'select * from first_t;\nselect * from second_t;';
  assert.deepEqual([...referencedTables(sql, 10).names], ['first_t']);
  assert.deepEqual([...referencedTables(sql, sql.length - 2).names], ['second_t']);
  // A semicolon inside a literal does not end a statement.
  assert.deepEqual([...referencedTables("select ';' from only_t").names], ['only_t']);
});

test('a derived table contributes its own FROM, not a name', () => {
  const { names } = referencedTables('select * from (select id from inner_t) z');
  assert.deepEqual([...names], ['inner_t'], 'the subquery has no name to offer');
});

test('a CTE reference is a table reference', () => {
  const { names } = referencedTables('with recent as (select id from sales.customer) select * from recent');
  assert.deepEqual([...names].sort(), ['recent', 'sales.customer']);
});

test('a half-typed FROM does not throw', () => {
  const full = 'select * from sales.customer c join sales.order_line o on o.id = c.id';
  for (let cut = 0; cut <= full.length; cut += 1) {
    assert.doesNotThrow(() => referencedTables(full.slice(0, cut)), `broke at ${cut}`);
  }
});

test('a half-typed CTE does not throw', () => {
  // Completion runs on every keystroke, so every prefix of a real query has to be
  // survivable.
  const full = 'with a as (select id, name from t) select * from a';
  for (let cut = 0; cut <= full.length; cut += 1) {
    assert.doesNotThrow(() => cteColumns(full.slice(0, cut), {}), `broke at ${cut}`);
  }
});
