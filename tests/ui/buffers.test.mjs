// Reordering tabs by drag. Run with:
//
//   node --test tests/ui/buffers.test.mjs
//
// Kept out of src/ui/ so rust-embed does not bake tests into the binary.

import assert from 'node:assert/strict';
import test from 'node:test';

import { reorder } from '../../src/ui/buffers.js';

const abc = ['a', 'b', 'c', 'd'];

test('dragging leftwards inserts before the target', () => {
  assert.deepEqual(reorder(abc, 3, 0), ['d', 'a', 'b', 'c']);
  assert.deepEqual(reorder(abc, 2, 1), ['a', 'c', 'b', 'd']);
});

test('dragging rightwards accounts for the item having left', () => {
  // The off-by-one this exists for: `to` indexes the list *before* the move, so
  // every index past `from` shifts down by one once it is removed.
  assert.deepEqual(reorder(abc, 0, 2), ['b', 'a', 'c', 'd']);
  assert.deepEqual(reorder(abc, 0, 4), ['b', 'c', 'd', 'a'], 'to the very end');
  assert.deepEqual(reorder(abc, 1, 3), ['a', 'c', 'b', 'd']);
});

test('dropping either side of itself changes nothing', () => {
  assert.deepEqual(reorder(abc, 1, 1), abc);
  assert.deepEqual(reorder(abc, 1, 2), abc);
});

test('the input is left alone', () => {
  const original = [...abc];
  reorder(abc, 0, 3);
  assert.deepEqual(abc, original);
});

test('a single tab cannot be moved anywhere', () => {
  assert.deepEqual(reorder(['only'], 0, 0), ['only']);
  assert.deepEqual(reorder(['only'], 0, 1), ['only']);
});
