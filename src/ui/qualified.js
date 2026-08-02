// Reading `source.database.table` out of otherwise ordinary SQL.
//
// This is the second way to change target, alongside `TARGET`: name the source in
// the query and alkyon points the editor at it, then sends the rest. The first
// part is the source, the second is the database, and **everything after that is
// left exactly as written** — because that is the part the engine has to
// understand, and alkyon does not translate.
//
//     select * from sales_db.warehouse.public.customer
//     └─ source ──┘ └─ db ──┘ └─ sent to the engine ─┘
//
// Only when the first part names a registered source, and only with three parts
// or more. Without that rule `alkyon_demo.sales.customer` — perfectly good
// three-part T-SQL — would be hijacked the moment someone registered a source
// called `alkyon_demo`.
//
// A folder or file source has **no databases** — DuckDB gives it one catalogue
// and that is that — so there is no database part to take, and its subdirectory
// comes straight after the source:
//
//     select * from taxi_data."2022".yellow_202212
//     └─ source ─┘ └─ sent as written ──────────┘

/** Quote characters that open an identifier, and what closes each. */
const QUOTES = { '"': '"', '`': '`', '[': ']' };

/**
 * Walk `sql`, yielding the identifier chains that appear in code.
 *
 * Strings, comments and the insides of quoted identifiers are skipped, so a
 * table name mentioned in a comment or inside a literal is never rewritten.
 */
function* chains(sql) {
  let i = 0;

  while (i < sql.length) {
    const c = sql[i];

    // Line comment.
    if (c === '-' && sql[i + 1] === '-') {
      const end = sql.indexOf('\n', i);
      i = end === -1 ? sql.length : end + 1;
      continue;
    }
    // Block comment. Not nested: no SQL dialect in play here nests them.
    if (c === '/' && sql[i + 1] === '*') {
      const end = sql.indexOf('*/', i + 2);
      i = end === -1 ? sql.length : end + 2;
      continue;
    }
    // String literal. `''` inside is an escaped quote, not the end.
    if (c === "'") {
      i += 1;
      while (i < sql.length) {
        if (sql[i] === "'") {
          if (sql[i + 1] === "'") i += 2;
          else {
            i += 1;
            break;
          }
        } else i += 1;
      }
      continue;
    }

    const chain = chainAt(sql, i);
    if (chain) {
      yield chain;
      i = chain.end;
      continue;
    }
    i += 1;
  }
}

/** One `a.b.c` chain starting at `start`, or null. */
function chainAt(sql, start) {
  const parts = [];
  let i = start;

  // A chain may not begin in the middle of a word.
  const before = sql[start - 1];
  if (before && /[A-Za-z0-9_$."`\]]/.test(before)) return null;

  for (;;) {
    const part = partAt(sql, i);
    if (!part) break;
    parts.push(part);
    i = part.end;
    if (sql[i] !== '.') break;
    i += 1;
  }

  if (parts.length === 0) return null;
  return { parts, start, end: i };
}

/** A bare or quoted identifier at `i`. */
function partAt(sql, i) {
  const close = QUOTES[sql[i]];
  if (close) {
    let j = i + 1;
    let name = '';
    while (j < sql.length) {
      if (sql[j] === close) {
        // A doubled closing quote is an escaped one.
        if (sql[j + 1] === close) {
          name += close;
          j += 2;
          continue;
        }
        return { name, start: i, end: j + 1 };
      }
      name += sql[j];
      j += 1;
    }
    return null;
  }

  const match = /^[A-Za-z_][A-Za-z0-9_$]*/.exec(sql.slice(i));
  if (!match) return null;
  return { name: match[0], start: i, end: i + match[0].length };
}

/**
 * The leading names of dotted chains that resolved to no source.
 *
 * A statement that says `taxi_data."2022".t` when the source is called
 * `taxi-data` is not retargeted at all: it goes to whatever the editor was
 * pointed at and fails there, with an error about a table nobody asked about.
 * Alkyon cannot warn about it up front — `alkyon_demo.sales.customer` looks
 * exactly the same and is ordinary SQL — but once the engine *has* refused, this
 * is what turns the error into a pointer.
 */
export function unresolvedHeads(sql, isKnownSource) {
  const heads = new Set();
  for (const { parts } of chains(sql)) {
    if (parts.length < 2) continue;
    if (!isKnownSource(parts[0].name)) heads.add(parts[0].name);
  }
  return [...heads];
}

/**
 * Find the source a statement names, and the SQL with that prefix removed.
 *
 * Returns `null` when nothing in the statement names a source — the ordinary
 * case, and the one that must cost nothing.
 *
 * @param {string} sql
 * @param {(id: string) => ({database: boolean} | null)} lookUp  null when the
 *        id names no source; `database: false` for a source that has none, so
 *        the part after it belongs to the name.
 * @returns {{source: string, database: string|null, sql: string}
 *          | {conflict: string[]}
 *          | null}
 */
export function findCrossSource(sql, lookUp) {
  /** @type {Array<{start:number,end:number,source:string,database:string|null,rest:string}>} */
  const found = [];

  for (const chain of chains(sql)) {
    const { parts } = chain;
    // Longest prefix wins: a source id may itself contain dots.
    for (let take = parts.length - 1; take >= 1; take -= 1) {
      const source = parts
        .slice(0, take)
        .map((p) => p.name)
        .join('.');
      const kind = lookUp(source);
      if (!kind) continue;

      // A source with databases eats the next part; one without does not, and
      // needs no more than `source.name` to be worth acting on.
      const consumed = kind.database ? take + 1 : take;
      if (parts.length <= consumed) break;

      found.push({
        start: chain.start,
        end: chain.end,
        source,
        database: kind.database ? parts[take].name : null,
        // Verbatim from the original text, quotes and all: this half is the
        // engine's business, not ours.
        rest: sql.slice(parts[consumed].start, chain.end),
      });
      break;
    }
  }

  if (found.length === 0) return null;

  const targets = [
    ...new Set(found.map((f) => (f.database ? `${f.source}/${f.database}` : f.source))),
  ];
  if (targets.length > 1) return { conflict: targets };

  // Rewrite from the back, so the earlier offsets stay valid.
  let rewritten = sql;
  for (const f of [...found].reverse()) {
    rewritten = rewritten.slice(0, f.start) + f.rest + rewritten.slice(f.end);
  }

  return { source: found[0].source, database: found[0].database, sql: rewritten };
}
