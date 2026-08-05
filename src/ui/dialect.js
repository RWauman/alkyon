// Per-dialect spelling: quoting identifiers, and writing a row-limited query.

/**
 * How many rows the explorer shows when you click a table.
 *
 * A page holds 50 000 by default, so this stays one page: the preview is meant to
 * answer "what is in here" in one round trip, not to start a paging session.
 */
export const PREVIEW_ROWS = 10_000;

/**
 * `SELECT * FROM <table>` limited to the first rows, in the dialect's own words.
 *
 * T-SQL is the odd one out: `TOP n` goes before the projection and `LIMIT` does
 * not exist at all.
 */
export function previewSql(dialect, qualified, rows = PREVIEW_ROWS) {
  return dialect === 'tsql'
    ? `SELECT TOP ${rows} * FROM ${qualified};`
    : `SELECT * FROM ${qualified} LIMIT ${rows};`;
}

/**
 * Quote an identifier the way the active engine expects.
 *
 * MySQL is the odd one out and it matters: a double-quoted name there is a
 * *string literal* unless the server runs with `ANSI_QUOTES`, so `"orders"`
 * would silently become the text "orders" rather than the table.
 *
 * @param {string} dialect  `tsql` | `pgsql` | `mysql` | `duckdb`
 */
export function quoteFor(dialect, name) {
  switch (dialect) {
    case 'tsql':
      return `[${name.replaceAll(']', ']]')}]`;
    case 'mysql':
      return `\`${name.replaceAll('`', '``')}\``;
    default:
      return `"${name.replaceAll('"', '""')}"`;
  }
}

/** Which dialect an editor mode belongs to. */
export function dialectForMime(mime) {
  switch (mime) {
    case 'text/x-mssql':
      return 'tsql';
    case 'text/x-pgsql':
      return 'pgsql';
    case 'text/x-mysql':
      return 'mysql';
    default:
      return 'duckdb';
  }
}

/** Whether SQL will read `name` as an identifier with nothing around it. */
export function isBareIdentifier(name) {
  return /^[A-Za-z_][A-Za-z0-9_$]*$/.test(name);
}

/**
 * Quote the parts of a dotted name that need it, and only those.
 *
 * A folder called `2022` becomes a schema called `2022`, which SQL reads as a
 * number — so `2022.trips` does not parse and `"2022".trips` does. Quoting every
 * segment would be safe too, but it turns every ordinary completion into
 * `"sales"."customer"`, and noise that is always there stops being read.
 *
 * `reserved` is the dialect's keyword set, so a table called `order` is quoted
 * for the same reason `2022` is.
 */
export function qualifyLoosely(dialect, dotted, reserved) {
  return dotted
    .split('.')
    .map((part) =>
      isBareIdentifier(part) && !reserved?.[part.toLowerCase()]
        ? part
        : quoteFor(dialect, part),
    )
    .join('.');
}
