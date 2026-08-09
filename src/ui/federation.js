// Reading a `DEFINE` block in the browser, so completion knows what the buffer
// declared.
//
// A second parser, and deliberately a *lenient* one. `federation::program` is the
// authority — it decides what runs — but it only ever sees a finished buffer,
// whereas this reads one that is being typed: half a declaration, an unclosed
// bracket, no `EVALUATE` yet. It reports what it can and ignores the rest, because
// the alternative is completion that switches off the moment you start editing.

/** `<KIND> <name> = <value>` at the start of a line. */
const DECLARATION = /^[ \t]*(ATTACH|IMPORT|FILES|EXCEL)[ \t]+(\w+)[ \t]*=[ \t]*(.*)$/i;

/**
 * The declarations of a federated buffer, in order.
 *
 * Each is `{ kind, alias, source, database, table, columns }`. The last two are how
 * an import's shape is known without running it: `table` when the query is nothing
 * but `select * from x`, so the columns are that table's; `columns` when the select
 * list names them itself, which it usually does.
 */
export function declarations(text) {
  const lines = String(text).split('\n');
  const stop = lines.findIndex((line) => /^[ \t]*EVALUATE\b/i.test(line));
  const block = stop === -1 ? lines : lines.slice(0, stop);

  const found = [];
  for (let index = 0; index < block.length; index += 1) {
    const match = DECLARATION.exec(block[index]);
    if (!match) continue;

    const [, keyword, alias, rest] = match;
    const kind = keyword.toUpperCase();
    // `AS (` may be on this line or the next, and the SQL after it may run on for
    // pages — everything up to the closing bracket belongs to this declaration.
    const [target, sql] = kind === 'IMPORT' ? splitAs(block, index, rest) : [rest, null];
    const { source, database } = splitTarget(kind, target);
    found.push({
      kind,
      alias,
      source,
      database,
      table: soleTable(sql),
      columns: projection(sql),
    });
  }
  return found;
}

/** The part before `AS`, and the bracketed SQL after it — which may span lines. */
function splitAs(lines, index, rest) {
  const here = /^(.*?)\bAS\b(.*)$/is.exec(rest);
  const target = here ? here[1] : rest;
  const tail = here ? here[2] : '';
  // Everything to the end of the block: the caller only wants to recognise the
  // simplest shape, so an unbalanced bracket costs nothing.
  const remainder = [tail, ...lines.slice(index + 1)].join('\n');
  const open = remainder.indexOf('(');
  if (open === -1) return [target, null];
  const close = remainder.lastIndexOf(')');
  return [target, remainder.slice(open + 1, close === -1 ? undefined : close)];
}

/**
 * The declaration whose native SQL surrounds `offset`, or null.
 *
 * What it is for: inside `IMPORT s = src AS ( … )` you are writing **the source's**
 * SQL, so the names that mean anything there are the source's tables — not the
 * aliases the buffer declares, which is what the surrounding query completes from.
 *
 * The scan is deliberately forgiving of an unclosed bracket, because one is open
 * for as long as it takes to type the query inside it.
 */
export function importAt(text, offset) {
  const source = String(text);
  const stop = source.search(/^[ \t]*EVALUATE\b/im);
  if (stop !== -1 && offset > stop) return null;

  const pattern = /^[ \t]*IMPORT[ \t]+(\w+)[ \t]*=([^\n]*?)\bAS\b/gim;
  let match = pattern.exec(source);
  while (match) {
    const open = source.indexOf('(', match.index + match[0].length);
    if (open !== -1 && (stop === -1 || open < stop)) {
      const close = closingBracket(source, open);
      const end = close === -1 ? (stop === -1 ? source.length : stop) : close;
      if (offset > open && offset <= end) {
        return { alias: match[1], from: open + 1, to: end };
      }
    }
    match = pattern.exec(source);
  }
  return null;
}

/**
 * The `)` that closes the `(` at `open`, stepping over everything SQL hides a
 * bracket inside. `-1` while it is still being typed.
 */
function closingBracket(text, open) {
  let depth = 0;
  for (let at = open; at < text.length; at += 1) {
    const c = text[at];
    if (c === "'" || c === '"' || c === '`') {
      at = text.indexOf(c, at + 1);
      if (at === -1) return -1;
    } else if (c === '[') {
      at = text.indexOf(']', at + 1);
      if (at === -1) return -1;
    } else if (c === '-' && text[at + 1] === '-') {
      const line = text.indexOf('\n', at);
      if (line === -1) return -1;
      at = line;
    } else if (c === '/' && text[at + 1] === '*') {
      const block = text.indexOf('*/', at + 2);
      if (block === -1) return -1;
      at = block + 1;
    } else if (c === '(') {
      depth += 1;
    } else if (c === ')') {
      depth -= 1;
      if (depth === 0) return at;
    }
  }
  return -1;
}

/**
 * `<source>[/<database>]`, except for `FILES` where the `/` starts a glob.
 */
function splitTarget(kind, target) {
  const value = target.trim();
  if (kind === 'FILES' || kind === 'EXCEL') {
    return { source: value.split('/')[0].trim(), database: null };
  }
  const cut = value.lastIndexOf('/');
  return cut === -1
    ? { source: value, database: null }
    : { source: value.slice(0, cut).trim(), database: value.slice(cut + 1).trim() };
}

/**
 * The one table a `select * from x` reads, or null for anything else.
 *
 * Deliberately narrow. Guessing the columns of a real query means running it or
 * describing it on the server; this covers the shape people actually write when
 * they just want a table, and admits it knows nothing about the rest.
 */
function soleTable(sql) {
  if (!sql) return null;
  // A quoted part may hold anything, including the space that makes it need
  // quoting in the first place.
  const match = /^\s*select\s+\*\s+from\s+((?:"[^"]*"|[\w.])+)\s*;?\s*$/i.exec(sql);
  return match ? match[1].replaceAll('"', '') : null;
}

/**
 * The columns an import will produce, read off its own select list.
 *
 * The names are right there in the query — `select id, name, 'x' as tab` produces
 * `id`, `name`, `tab` — so completion can offer them without describing anything
 * on the server. What it cannot name it leaves out rather than guessing: a `*`
 * belongs to a table this does not resolve, and a bare `count(*)` has no name
 * until someone gives it one.
 */
export function projection(sql) {
  if (!sql) return null;
  const select = /^\s*select\s+(?:distinct\s+|all\s+)?/i.exec(sql);
  if (!select) return null;

  const start = select[0].length;
  const from = topLevel(sql, start, /^from\b/i);
  const list = sql.slice(start, from === -1 ? sql.length : from);

  const names = [];
  for (const item of split(list)) {
    const name = outputName(item);
    if (name && !names.includes(name)) names.push(name);
  }
  return names.length ? names : null;
}

/** The name an item of a select list will have in the result. */
function outputName(item) {
  const text = item.trim().replace(/;$/, '').trim();
  if (!text || text.endsWith('*')) return null;

  // `… AS name` wins, and it is the last top-level one.
  const as = topLevel(text, 0, /^as\b/i, true);
  if (as !== -1) {
    const named = text.slice(as + 2).trim();
    return /^(?:"[^"]*"|\[[^\]]*\]|`[^`]*`|\w+)$/.test(named)
      ? named.replace(/^["[`]|["\]`]$/g, '')
      : null;
  }
  // Otherwise a plain reference names itself, and its last part is the column.
  if (!/^(?:"[^"]*"|[\w.])+$/.test(text)) return null;
  const parts = text.split('.');
  return parts[parts.length - 1].replaceAll('"', '') || null;
}

/** Top-level commas only — a `coalesce(a, b)` is one item, not two. */
function split(list) {
  const items = [];
  let at = 0;
  for (let index = 0; index <= list.length; index += 1) {
    if (index === list.length) {
      items.push(list.slice(at));
      break;
    }
    const skipped = skip(list, index);
    if (skipped !== index) {
      index = skipped;
      continue;
    }
    if (list[index] === ',') {
      items.push(list.slice(at, index));
      at = index + 1;
    }
  }
  return items;
}

/** The first top-level match of `word` at or after `from`, or -1. `last` for the last. */
function topLevel(text, from, word, last = false) {
  let found = -1;
  for (let index = from; index < text.length; index += 1) {
    const skipped = skip(text, index);
    if (skipped !== index) {
      index = skipped;
      continue;
    }
    const before = index === 0 || /\s|[(,]/.test(text[index - 1]);
    if (before && word.test(text.slice(index))) {
      if (!last) return index;
      found = index;
    }
  }
  return found;
}

/**
 * The index after whatever starts at `at` that is not code — a string, a quoted
 * identifier, a comment, a bracketed group — or `at` itself when it is code.
 */
function skip(text, at) {
  const c = text[at];
  if (c === "'" || c === '"' || c === '`') {
    const end = text.indexOf(c, at + 1);
    return end === -1 ? text.length : end;
  }
  if (c === '[') {
    const end = text.indexOf(']', at + 1);
    return end === -1 ? text.length : end;
  }
  if (c === '-' && text[at + 1] === '-') {
    const end = text.indexOf('\n', at);
    return end === -1 ? text.length : end;
  }
  if (c === '/' && text[at + 1] === '*') {
    const end = text.indexOf('*/', at + 2);
    return end === -1 ? text.length : end + 1;
  }
  if (c === '(') {
    let depth = 0;
    for (let index = at; index < text.length; index += 1) {
      const inner = index === at ? index : skip(text, index);
      if (inner !== index) {
        index = inner;
        continue;
      }
      if (text[index] === '(') depth += 1;
      else if (text[index] === ')') {
        depth -= 1;
        if (depth === 0) return index;
      }
    }
    return text.length;
  }
  return at;
}
