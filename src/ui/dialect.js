// Per-dialect spelling: quoting identifiers, and writing a row-limited query.

/** How many rows the explorer shows when you click a table. */
export const PREVIEW_ROWS = 100;

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
