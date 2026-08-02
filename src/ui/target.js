// Parsing for the `TARGET` directive. Pure — no DOM — so the dialect-safety
// rules can be tested on their own; see tests/ui/target.test.mjs.

/**
 * Matched only as the *first* token of what is left of the buffer.
 *
 * Three words were considered and two rejected:
 *
 *   - `USE` is reserved in T-SQL and a real statement in MySQL, DuckDB and
 *     ClickHouse.
 *   - `SOURCE` is the MySQL client's own include command — `SOURCE file.sql`,
 *     written at the start of a line, exactly where this directive lives. It
 *     also reads wrongly here: a *source* is the registered thing, so
 *     `SOURCE pg-dev` sounds like declaring one rather than pointing at it.
 *   - `TARGET` appears in no dialect's keyword list, begins no statement
 *     anywhere, and says what the line does. `MERGE … MATCHED BY TARGET` uses
 *     the word mid-statement, which this rule cannot reach.
 *
 * `SWAP` is still accepted so that files written before the rename keep working.
 */
const DIRECTIVE = /^\s*(?:TARGET|SWAP)\s+([A-Za-z0-9._:-]+)\s*(?:;|$|\n)/i;

/**
 * Blank lines and `--` comments ahead of a directive.
 *
 * "First token" cannot mean "first character": the editor opens on lines of
 * instructions, one of which explains this very directive, and every `.sql` file
 * anyone keeps starts with a header. Requiring it at character zero meant the
 * directive did not work in the buffer that documents it.
 *
 * The server already reads `-- @duckdb` this way, so the two rules now agree.
 */
const LEADING_COMMENTS = /^(?:[ \t]*(?:--[^\n]*)?\r?\n)*/;

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
 * Is the cursor typing the *target* of a `TARGET` directive?
 *
 * A directive is only a directive as the first token of what is left of the
 * buffer, so this walks the leading lines the same way [`parseTarget`] does and
 * stops at the first that is not one. Returns the partial token and where it
 * starts, or `null`.
 *
 * @param {string[]} lines  the buffer, split
 * @param {number} line     the cursor's line
 * @param {number} ch       the cursor's column
 */
export function targetAt(lines, line, ch) {
  // Every line above must be a directive, a comment or blank, or this one is
  // ordinary SQL that merely looks like a directive.
  for (let above = 0; above < line; above += 1) {
    const text = lines[above].trim();
    if (text === '' || text.startsWith('--')) continue;
    if (!DIRECTIVE.test(`${text}\n`)) return null;
  }

  const text = lines[line] ?? '';
  const opening = /^(\s*(?:TARGET|SWAP)\s+)([A-Za-z0-9._:-]*)/i.exec(text);
  if (!opening) return null;

  const start = opening[1].length;
  const end = start + opening[2].length;
  // Only while the cursor is inside the target, not past the end of it.
  if (ch < start || ch > end) return null;
  return { token: text.slice(start, ch), start, end };
}

/**
 * Peel every leading TARGET directive off `sql`.
 *
 * @param {string} sql
 * @param {(id: string) => boolean} isKnownSource
 * @returns {{targets: Array<{id: string, database?: string}>, sql: string}
 *          | {unknown: string}} the directives and the SQL left to run, or the
 *          token that named nothing.
 */
export function parseTarget(sql, isKnownSource) {
  const targets = [];
  let rest = sql;
  // Comments stepped over on the way to a directive. Put back afterwards: they
  // are the user's, and dropping them would also shift every line number in the
  // errors the engine reports.
  let comments = '';

  for (;;) {
    const skipped = LEADING_COMMENTS.exec(rest)[0];
    const after = rest.slice(skipped.length);
    const match = DIRECTIVE.exec(after);
    if (!match) break;

    const target = resolve(match[1], isKnownSource);
    if (!target) return { unknown: match[1] };
    targets.push(target);
    comments += skipped;
    rest = after.slice(match[0].length);
  }

  return { targets, sql: (comments + rest).trim() };
}
