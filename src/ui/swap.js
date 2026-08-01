// Parsing for the `SWAP` directive. Pure — no DOM — so the dialect-safety rules
// can be tested on their own; see tests/ui/swap.test.mjs.

/**
 * Matched only as the *first* token of what is left of the buffer.
 *
 * `SWAP` is not a keyword in PostgreSQL, SQL Server, MySQL or MariaDB. Snowflake
 * is the one dialect that uses the word — `ALTER TABLE a SWAP WITH b` — and never
 * puts it first, so anchoring here cannot shadow real SQL as engines are added.
 * `USE` was the obvious candidate and is unusable: reserved in T-SQL, and a real
 * statement in MySQL, DuckDB and ClickHouse.
 */
const DIRECTIVE = /^\s*SWAP\s+([A-Za-z0-9._:-]+)\s*(?:;|$|\n)/i;

/**
 * Source ids may contain dots, so the whole token is tried as an id before a
 * trailing `.database` is split off it.
 */
function resolve(token, isKnownSource) {
  if (isKnownSource(token)) return { id: token };

  const cut = token.lastIndexOf('.');
  if (cut > 0) {
    const id = token.slice(0, cut);
    if (isKnownSource(id)) return { id, database: token.slice(cut + 1) };
  }
  return null;
}

/**
 * Peel every leading SWAP directive off `sql`.
 *
 * @param {string} sql
 * @param {(id: string) => boolean} isKnownSource
 * @returns {{targets: Array<{id: string, database?: string}>, sql: string}
 *          | {unknown: string}} the directives and the SQL left to run, or the
 *          token that named nothing.
 */
export function parseSwap(sql, isKnownSource) {
  let rest = sql.trim();
  const targets = [];

  for (let match = DIRECTIVE.exec(rest); match; match = DIRECTIVE.exec(rest)) {
    const target = resolve(match[1], isKnownSource);
    if (!target) return { unknown: match[1] };
    targets.push(target);
    rest = rest.slice(match[0].length).trim();
  }

  return { targets, sql: rest };
}
