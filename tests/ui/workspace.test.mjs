// What the folder pane's type filter says it is showing. Run with:
//
//   node --test tests/ui/workspace.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { typeLabel } from '../../src/ui/workspace.js';

const folder = ['csv', 'json', 'parquet', 'sql', 'xlsx'];
const label = (...hidden) => typeLabel(folder, new Set(hidden));

test('nothing hidden is `all`, however many types there are', () => {
  assert.equal(label(), 'all');
  assert.equal(typeLabel([], new Set()), 'all');
});

test('one or two types left are named', () => {
  assert.equal(label('csv', 'json', 'parquet', 'xlsx'), 'sql');
  assert.equal(label('json', 'parquet', 'xlsx'), 'csv sql');
});

test('three or more are counted rather than listed', () => {
  assert.equal(label('json', 'xlsx'), '3 types');
  assert.equal(label('xlsx'), '4 types');
});

test('hiding everything says so', () => {
  assert.equal(label(...folder), 'none');
});

/**
 * A type hidden while another folder was open is not this folder's business. The
 * label has to come from what is on screen, or the button reads `5 types` over a
 * tree that is in fact showing everything it has.
 */
test('a type this folder does not hold changes nothing', () => {
  assert.equal(typeLabel(['sql'], new Set(['ndjson'])), 'all');
  assert.equal(label('ndjson'), 'all');
});
