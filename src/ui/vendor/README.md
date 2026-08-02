# Vendored UI libraries

Committed rather than fetched, because the workbench has to work offline and
because `cargo build` should be the whole build — there is no npm step. All of
them are MIT licensed; the licence texts sit next to the files. Alkyon itself is
not MIT — see the [licence](../../../README.md#licence) — and these keep their
own terms.

| Library | Version | Files |
|---|---|---|
| [CodeMirror](https://codemirror.net/5/) | 5.65.21 | `codemirror.min.*`, `cm-*.min.*` |
| [glide-data-grid](https://github.com/glideapps/glide-data-grid) | 6.0.3 | `glide-data-grid.min.js`, `glide-data-grid.css` — a bundle, built by `tools/grid` |
| [xterm.js](https://xtermjs.org/) | 6.0.0 | `xterm.js`, `xterm.css` |
| [xterm addon-fit](https://github.com/xtermjs/xterm.js) | 0.11.0 | `xterm-addon-fit.js` |

CodeMirror is on the frozen 5.x line, not 6.x: 6 is ESM-only and would require a
bundler. Its `sql` mode already speaks `text/x-mssql` and `text/x-pgsql`, and
`sql-hint` takes a `{ "schema.table": ["column"] }` map, which is exactly the
shape the explorer produces.

To refresh a version, replace the file from
`https://cdn.jsdelivr.net/npm/<package>@<version>/<path>` and update this table.
