// Reading the CTEs out of a buffer, so completion knows about names that exist
// only in the text you are writing.
//
// **This is not a SQL parser and must not pretend to be one.** It knows enough to
// find `WITH name AS ( … )`, to split a select list on its top-level commas, and to
// take the name off each item. Where it cannot tell, it says nothing rather than
// guessing — an absent completion costs a keystroke, a wrong one costs a debugging
// session.
//
// What it handles: several CTEs, an explicit `(a, b, c)` column list, `AS
// MATERIALIZED`, `RECURSIVE`, aliases with or without `AS`, dotted names,
// quoted identifiers, nested parentheses, and `SELECT *` when the `FROM` names a
// table whose columns are already known.
//
// What it does not: set operations (`UNION`) beyond their first branch, `t.*`,
// expressions with no alias, and window or lateral shapes where the name is not
// syntactic. Each of those yields *fewer* completions, never wrong ones.

/**
 * Blank out comments and string literals, keeping the length identical.
 *
 * Every scan below counts brackets and commas, and an apostrophe in a comment or a
 * bracket in a string would throw all of it off. Replacing rather than removing
 * means every offset still lines up with the original text.
 */
export function mask(sql) {
  const out = sql.split('');
  let at = 0;
  const blank = (from, to) => {
    for (let i = from; i < to && i < out.length; i += 1) {
      if (out[i] !== '\n') out[i] = ' ';
    }
  };

  while (at < sql.length) {
    const two = sql.slice(at, at + 2);
    if (two === '--') {
      const end = sql.indexOf('\n', at);
      const stop = end === -1 ? sql.length : end;
      blank(at, stop);
      at = stop;
    } else if (two === '/*') {
      const end = sql.indexOf('*/', at + 2);
      const stop = end === -1 ? sql.length : end + 2;
      blank(at, stop);
      at = stop;
    } else if (sql[at] === "'" || sql[at] === '"' || sql[at] === '`' || sql[at] === '[') {
      const close = sql[at] === '[' ? ']' : sql[at];
      let end = at + 1;
      while (end < sql.length) {
        if (sql[end] === close) {
          // A doubled quote is an escaped one and the literal carries on.
          if (sql[end + 1] === close && close !== ']') end += 2;
          else break;
        } else {
          end += 1;
        }
      }
      // The delimiters stay, so a quoted name is still recognisable as a token.
      blank(at + 1, end);
      at = end + 1;
    } else {
      at += 1;
    }
  }
  return out.join('');
}

/** The index just past `(`'s match, given `open` points at the `(`. */
function afterGroup(masked, open) {
  let depth = 0;
  for (let at = open; at < masked.length; at += 1) {
    if (masked[at] === '(') depth += 1;
    else if (masked[at] === ')') {
      depth -= 1;
      if (depth === 0) return at + 1;
    }
  }
  return -1;
}

/** Split on commas that are not inside brackets. */
function topLevelCommas(masked, text) {
  const parts = [];
  let depth = 0;
  let start = 0;
  for (let at = 0; at < masked.length; at += 1) {
    const c = masked[at];
    if (c === '(') depth += 1;
    else if (c === ')') depth -= 1;
    else if (c === ',' && depth === 0) {
      parts.push(text.slice(start, at));
      start = at + 1;
    }
  }
  parts.push(text.slice(start));
  return parts.map((part) => part.trim()).filter(Boolean);
}

/** Strip one layer of quoting from an identifier. */
function unquote(name) {
  const first = name[0];
  if (first === '"' || first === '`') return name.slice(1, -1).replaceAll(first.repeat(2), first);
  if (first === '[') return name.slice(1, -1);
  return name;
}

/** One identifier: quoted, bracketed, or bare. */
const SEGMENT = '(?:"(?:[^"]|"")*"|`(?:[^`]|``)*`|\\[[^\\]]*\\]|[A-Za-z_][A-Za-z0-9_$]*)';
const IDENTIFIER = new RegExp(`^${SEGMENT}`);
/** A whole dotted path and nothing else: `id`, `c.name`, `"odd name"`. */
const PATH_ONLY = new RegExp(`^${SEGMENT}(?:\\s*\\.\\s*${SEGMENT})*$`);
/** A dotted path at the start of the text, however it continues. */
const PATH_START = new RegExp(`^${SEGMENT}(?:\\s*\\.\\s*${SEGMENT})*`);
const ALIAS_AT_END = new RegExp(`(\\bas\\s+|\\s)(${SEGMENT})\\s*$`, 'i');

/** Words that can only be a keyword where an alias would otherwise sit. */
const NOT_AN_ALIAS =
  /^(from|where|group|order|having|union|except|intersect|limit|offset|window|qualify|by|on|and|or|desc|asc)$/i;

/**
 * Whether an implicit alias — `sum(x) total`, with no `AS` — can follow this text.
 *
 * It can only follow a *finished* expression. `a + b` ends in an operator, so `b`
 * is an operand and not a name; `sum(x)` ends in a bracket, so what comes next is
 * a name. Without this check every arithmetic expression donated its last term as
 * a column that does not exist.
 */
function endsAnExpression(before) {
  const last = before.trimEnd().slice(-1);
  return last !== '' && !'+-*/%|&^~<>=,(.'.includes(last);
}

/**
 * The name one select-list item will have in the result.
 *
 * `count(*) as n` → `n`; `c.name` → `name`; `count(*)` → null, because it has no
 * name a completion could offer.
 */
export function outputName(item) {
  const text = item.trim();
  if (!text) return null;

  // A bare reference names itself — `id`, `c.name`, `"odd name"`. Tested first
  // because a single quoted identifier has no alias in front of it to find.
  if (PATH_ONLY.test(text)) {
    const segments = text.split(/\s*\.\s*/);
    return unquote(segments[segments.length - 1]);
  }

  // Otherwise the name is the last word, with or without `AS`.
  const found = mask(text).match(ALIAS_AT_END);
  if (!found) return null;
  const start = found.index + found[0].length - found[2].length;
  const name = unquote(item.trim().slice(start, start + found[2].length));
  if (NOT_AN_ALIAS.test(name)) return null;

  // `AS` says outright that this is a name; without it, only a finished
  // expression can be followed by one.
  const explicit = /^as\s/i.test(found[1].trimStart());
  return explicit || endsAnExpression(text.slice(0, found.index + 1)) ? name : null;
}

/**
 * The columns a `SELECT` body produces, and the first table it reads from.
 *
 * `from` comes back so the caller can resolve a bare `*` against a schema it
 * already knows — `with x as (select * from sales.customer)` being the shape this
 * would otherwise be useless for.
 */
export function selectShape(body) {
  const masked = mask(body);

  // The first top-level `select`, so a leading `(` or a nested one is skipped.
  const select = topLevelKeyword(masked, 'select');
  if (select === -1) return { columns: [], from: null, star: false };

  let at = select + 'select'.length;
  // `distinct`, `all`, `top n`, `distinct on (…)` sit between select and the list.
  const lead = /^\s*(?:all\b|distinct\b(?:\s*on\s*)?|top\s+\d+\b|top\s*\(\s*\d+\s*\)\b)*/i;
  const skipped = masked.slice(at).match(lead);
  at += skipped ? skipped[0].length : 0;
  if (masked[skipStart(masked, at)] === '(') {
    // `distinct on (a, b)` — step over the group before the list starts.
    const open = skipStart(masked, at);
    const past = afterGroup(masked, open);
    if (past !== -1) at = past;
  }

  const from = topLevelKeyword(masked.slice(at), 'from');
  const listEnd = from === -1 ? masked.length : at + from;
  const items = topLevelCommas(masked.slice(at, listEnd), body.slice(at, listEnd));

  const star = items.some((item) => item.trim() === '*');
  const columns = items.map(outputName).filter(Boolean);

  let table = null;
  if (from !== -1) {
    const after = body.slice(at + from + 'from'.length).trimStart();
    const name = after.match(
      /^(?:"(?:[^"]|"")*"|`(?:[^`]|``)*`|\[[^\]]*\]|[A-Za-z_][A-Za-z0-9_$]*)(?:\s*\.\s*(?:"(?:[^"]|"")*"|`(?:[^`]|``)*`|\[[^\]]*\]|[A-Za-z_][A-Za-z0-9_$]*))*/,
    );
    if (name) table = name[0].split('.').map((part) => unquote(part.trim())).join('.');
  }
  return { columns, from: table, star };
}

/** First non-space index at or after `at`. */
function skipStart(masked, at) {
  let i = at;
  while (i < masked.length && /\s/.test(masked[i])) i += 1;
  return i;
}

/** Index of `word` as a whole word at bracket depth zero, or -1. */
function topLevelKeyword(masked, word) {
  const pattern = new RegExp(`\\b${word}\\b`, 'gi');
  let depth = 0;
  const depths = new Array(masked.length);
  for (let at = 0; at < masked.length; at += 1) {
    depths[at] = depth;
    if (masked[at] === '(') depth += 1;
    else if (masked[at] === ')') depth -= 1;
  }
  let found = pattern.exec(masked);
  while (found) {
    if (depths[found.index] === 0) return found.index;
    found = pattern.exec(masked);
  }
  return -1;
}

/**
 * Every CTE in the buffer: its name, the columns it produces, and — when the
 * columns could not be worked out — the table it selects from, so the caller can
 * fall back to a schema it knows.
 */
export function parseCtes(sql) {
  const masked = mask(sql);
  const ctes = [];

  // `with` at the start of a statement. A `with` inside a subquery is somebody
  // else's scope and its names are not visible here.
  const starts = /(^|;)\s*with\s+(recursive\s+)?/gi;
  let head = starts.exec(masked);
  while (head) {
    let at = head.index + head[0].length;

    for (;;) {
      // The whitespace has to go first: `IDENTIFIER` is anchored, so a leading
      // space after `,` made every CTE past the first one invisible.
      at = skipStart(masked, at);
      const name = masked.slice(at).match(IDENTIFIER);
      if (!name) break;
      const cteName = unquote(sql.slice(at, at + name[0].length));
      at += name[0].length;

      // An optional explicit column list, which is the exact answer when present.
      let declared = null;
      let cursor = skipStart(masked, at);
      if (masked[cursor] === '(') {
        const past = afterGroup(masked, cursor);
        if (past === -1) break;
        const inner = sql.slice(cursor + 1, past - 1);
        declared = topLevelCommas(mask(inner), inner).map((one) => unquote(one.trim()));
        at = past;
        cursor = skipStart(masked, at);
      }

      // `AS`, then an optional MATERIALIZED, then the body.
      const as = masked.slice(cursor).match(/^as\s+(?:not\s+materialized\s+|materialized\s+)?/i);
      if (!as) break;
      cursor = skipStart(masked, cursor + as[0].length);
      if (masked[cursor] !== '(') break;
      const past = afterGroup(masked, cursor);
      if (past === -1) break;

      const body = sql.slice(cursor + 1, past - 1);
      const shape = declared
        ? { columns: declared, from: null, star: false }
        : selectShape(body);
      ctes.push({ name: cteName, columns: shape.columns, from: shape.from, star: shape.star });

      at = past;
      cursor = skipStart(masked, at);
      if (masked[cursor] !== ',') break;
      at = cursor + 1;
    }

    head = starts.exec(masked);
  }
  return ctes;
}

/** Words that end a table list rather than naming or aliasing a table. */
const ENDS_TABLE_LIST =
  /^(?:where|group|order|having|union|except|intersect|limit|offset|on|using|join|inner|left|right|full|cross|outer|natural|lateral|window|qualify|select|set|values|returning|with|for|option|by|and|or)$/i;

/** The statement the cursor sits in, as `[start, end)` offsets into `sql`. */
function statementAround(masked, offset) {
  let depth = 0;
  let start = 0;
  for (let at = 0; at < masked.length; at += 1) {
    const c = masked[at];
    if (c === '(') depth += 1;
    else if (c === ')') depth -= 1;
    else if (c === ';' && depth === 0) {
      if (at >= offset) return [start, at];
      start = at + 1;
    }
  }
  return [start, masked.length];
}

/**
 * The tables one statement reads from, as written.
 *
 * This is what scopes column completion: offering every column of a 285-column
 * database when the query names two tables is not help, it is a haystack. Aliases
 * come back alongside so `c` in `from sales.customer c` resolves too.
 *
 * `from` and `join` are collected at **any** bracket depth. A subquery's tables are
 * in scope inside it, and following the cursor's exact scope would mean tracking
 * nesting for a gain nobody would notice — being a little generous here costs a few
 * extra suggestions, being strict would cost the right ones.
 */
export function referencedTables(sql, offset = sql.length) {
  const masked = mask(sql);
  const [start, end] = statementAround(masked, Math.min(offset, masked.length));
  const region = masked.slice(start, end);

  const names = new Set();
  const aliases = new Map();
  const keyword = /\b(?:from|join)\b/gi;

  let found = keyword.exec(region);
  while (found) {
    let at = found.index + found[0].length;

    for (;;) {
      at = skipStart(region, at);
      if (at >= region.length) break;

      if (region[at] === '(') {
        // A derived table: no name to record, and its own FROM is found by this
        // same scan because the search is depth-blind.
        const past = afterGroup(region, at);
        if (past === -1) break;
        at = past;
      } else {
        const path = region.slice(at).match(PATH_START);
        if (!path) break;
        const written = sql.slice(start + at, start + at + path[0].length);
        const name = written
          .split('.')
          .map((part) => unquote(part.trim()))
          .join('.');
        if (ENDS_TABLE_LIST.test(name)) break;
        names.add(name);
        at += path[0].length;

        // An alias, with or without `AS`, unless what follows is a keyword.
        const after = region.slice(at).match(/^\s*(?:as\s+)?/i);
        const candidateAt = skipStart(region, at + (after ? after[0].length : 0));
        const alias = region.slice(candidateAt).match(IDENTIFIER);
        if (alias && !ENDS_TABLE_LIST.test(alias[0])) {
          aliases.set(unquote(sql.slice(start + candidateAt, start + candidateAt + alias[0].length)), name);
          at = candidateAt + alias[0].length;
        }
      }

      at = skipStart(region, at);
      if (region[at] !== ',') break;
      at += 1;
    }

    found = keyword.exec(region);
  }
  return { names, aliases };
}

/**
 * CTE name → column names, resolving a bare `*` against `known` (the schema map
 * completion already holds) when it can.
 *
 * A CTE that selects from another CTE resolves too, because the earlier one is
 * already in the map by the time the later one is looked up.
 */
export function cteColumns(sql, known = {}) {
  const out = {};
  for (const cte of parseCtes(sql)) {
    let columns = [...cte.columns];
    if (cte.star && cte.from) {
      const source = known[cte.from] ?? out[cte.from];
      if (source) columns = [...source, ...columns];
    }
    // Deduplicated, keeping the order they were written in.
    out[cte.name] = [...new Set(columns)];
  }
  return out;
}
