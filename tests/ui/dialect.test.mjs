// Identifier quoting per dialect. Run with:
//
//   node --test tests/ui/dialect.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { previewSql, quoteFor } from '../../src/ui/dialect.js';

test('each engine gets the quoting it actually accepts', () => {
  assert.equal(quoteFor('tsql', 'orders'), '[orders]');
  assert.equal(quoteFor('pgsql', 'orders'), '"orders"');
  assert.equal(quoteFor('duckdb', 'orders'), '"orders"');
  // The one that matters: in MySQL a double-quoted name is a *string literal*
  // unless the server runs with ANSI_QUOTES, so `"orders"` would silently be the
  // text rather than the table.
  assert.equal(quoteFor('mysql', 'orders'), '`orders`');
});

test('a name containing the quote character is escaped, not broken', () => {
  assert.equal(quoteFor('tsql', 'we]rd'), '[we]]rd]');
  assert.equal(quoteFor('pgsql', 'we"rd'), '"we""rd"');
  assert.equal(quoteFor('mysql', 'we`rd'), '`we``rd`');
});

test('an unknown dialect falls back to the SQL standard', () => {
  assert.equal(quoteFor(undefined, 'orders'), '"orders"');
});

test('the preview limits rows the way each engine spells it', () => {
  // T-SQL has no LIMIT, and TOP goes before the projection rather than at the end.
  assert.equal(previewSql('tsql', '[sales].[customer]'), 'SELECT TOP 100 * FROM [sales].[customer];');
  assert.equal(previewSql('pgsql', '"sales"."customer"'), 'SELECT * FROM "sales"."customer" LIMIT 100;');
  assert.equal(previewSql('mysql', '`sales`.`customer`'), 'SELECT * FROM `sales`.`customer` LIMIT 100;');
  assert.equal(previewSql('duckdb', '"main"."trips"'), 'SELECT * FROM "main"."trips" LIMIT 100;');
  assert.equal(previewSql('tsql', '[t]', 5), 'SELECT TOP 5 * FROM [t];');
});
