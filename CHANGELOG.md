# Changelog

What changed between releases, newest first. The notes attached to a tag are cut
from the entry above it, so this is where a change gets written down **when it is
made**, not when it is published — a release then costs a heading and a date rather
than an evening spent reading diffs.

Dates are the day the tag was cut. Anything under **Unreleased** is on `dev` and in
no binary yet.

## Unreleased

### Folder sources, rebuilt around directories

- **A file at the root is a table, and a subdirectory is one table over its files** —
  unioned, with a `source_file` column naming the file each row came from. A folder
  of monthly exports is one table and not twelve, and `customers.parquet` beside it
  is still just `customers`.
- **The catalogue is named after the folder**, not `memory`, and every table lives in
  `public` as it would in psql: `parquet → public → customers`.
- **The file type is declared, not guessed.** It is the first field in the dialogue,
  a folder source is refused without it, and only that type's options are shown — a
  delimiter means nothing to a parquet file. Files of two types cannot be one table,
  which is why there is no longer a "work it out per file".
- **Files can be left out.** The dialogue lists what the folder holds and ticking a
  file stops the source reading it; a subdirectory left with nothing stops being a
  table. The list keeps offering the excluded ones, or they could never come back.
- **The files of one subdirectory must have exactly the same columns.** Rigid on
  purpose: a mismatch is an error naming the file, not a reshaping that quietly
  drops a column. For spreadsheets, where calamine does the reading rather than
  DuckDB, alkyon makes that check itself.
- **A workbook is one table per sheet**, and a subdirectory of workbooks unions a
  named sheet across them.
- `GET /files?path=&format=` lists what a folder holds, which is what fills the
  dialogue's list.

### Azure

- **Sign in to Microsoft Entra from the dialogue.** *Sign in…* opens the browser,
  takes the redirect on a loopback port, and comes back with the account named —
  authorization code with PKCE, no client secret, nothing pasted. **Device code**
  is the same sign-in for when the browser is somewhere else, which is what makes
  it work under Docker.
- **What is kept is the refresh token**, in the keychain, and an access token is
  minted from it when a connection opens. A signed-in source keeps working past
  the hour an access token lasts — the whole point over pasting one in. Rotated
  refresh tokens replace the old one, and an expired sign-in says so instead of
  failing as a connection error.
- **Tenant and application id are configurable**, defaulting to `organizations`
  and Azure CLI's public client so that signing in works before anyone has
  registered anything.
- **The page never holds a token.** The sign-in finishes server-side and the
  dialogue quotes a one-use ticket; the tokens go straight to the keychain.
- **Azure storage is a source**: blob containers, ADLS Gen2 filesystems and Fabric
  OneLake, which are one API under three names. It reads as a folder source in
  every respect — catalogue, `public`, a table per root file, a subdirectory
  unioned with `source_file`, spreadsheets per sheet, `@import`.
- **It syncs rather than querying remotely**, because the sandboxed DuckDB has no
  network access and can load no extension. Files are mirrored locally, kept
  between runs, and re-fetched only when their ETag changes; there is no predicate
  pushdown, and the guide says so plainly.
- **Entra, Windows integrated and pasted tokens are no longer offered for
  PostgreSQL and MySQL**, whose connectors have only ever accepted a password.
- **A routing token is followed.** Azure SQL and Fabric answer the login by
  pointing at the node that actually holds the database — for a Fabric endpoint,
  every time — and that arrived as *Server requested a connection to an
  alternative address*. The routed name is split on its backslash, as Microsoft's
  own Go driver does: the host is dialled on the port the token carries, and the
  instance is dropped rather than sent to a SQL Browser that is not there. One
  redirect, never a loop.
- **A hostname in *Named instance* is refused with a sentence saying where it
  belongs.** Naming an instance sends the connection to the SQL Browser on UDP
  1434, which no cloud endpoint runs, so pasting a Fabric endpoint there produced
  a browser timeout accusing a host that answers perfectly well.

### What does not work

- **A Fabric SQL endpoint still cannot be connected to**, and the guide has a
  section saying why. The sign-in, the token and the first login all succeed;
  the login on the node Fabric routes to does not. `tiberius` takes one field
  for both the TLS name and the login name, and after a Fabric redirect those
  have to differ — host for the certificate, host *and* instance for the login.
  Azure SQL is unaffected: its redirects carry no instance. Until this is
  settled, the same lakehouse is readable through an Azure storage source over
  OneLake.
- **A Delta table is read as a plain folder of parquet.** The `_delta_log` is
  ignored, so a lakehouse's `Tables/` is right only for append-only tables and
  wrong after an update or a delete. `Files/` is unaffected.
- **The Azure storage path has not been run end to end** against a real account
  — the parts are tested and the error paths were exercised against the live
  service, but no query has yet come back from a mirrored blob.

### The result grid

- **A filter per column.** Hover a header and click the **▾**: plain text matches
  anywhere in the cell, and a leading `>= != <`… compares — numerically on a numeric
  column, which arrives as text to keep its digits. Below the box, whatever the
  column can offer: values with counts for a short list, two ends and two sliders
  for a measure, year → month → day for a date. Built from every row the page holds
  rather than a sample, and it stops looking past 500 distinct values. A filtered
  column is marked **⌕** and the footer says how many rows are left. It filters
  **the page in memory**, the same scope as the sort, rather than quietly re-running
  the query.
- **No limit** joins the page sizes: one page holding the whole result.
- **Click a table in the explorer** and its first 10 000 rows open in a preview tab
  of their own, so it neither touches what you were writing nor throws away the
  result you were looking at.

### The editor

- **Completion knows your CTEs.** `with recent as (select id, name from …)` offers
  `recent.id` afterwards. Deliberately not a SQL parser: where it cannot tell, it
  offers nothing rather than guessing.
- **Tables and columns are both offered, whatever the clause**, and columns are
  scoped to what the statement actually reads from.

### The sidebar and the tabs

- **Three collapsible sections** — Search, Sources, Folder — draggable to resize,
  and remembered as you left them.
- **↻ on a source row re-reads that one source**: new files, new tables, and the
  schema behind completion, without re-probing every server in the sidebar.
- **✕ asks first.** In alkyon's own dialogue rather than the browser's `confirm`,
  which Chrome offers to silence for the rest of a session — and a silenced
  confirmation is a source deleted without being asked.
- **Drag a tab to reorder it.**
- **One click opens a `.sql` file**, or goes to the tab it is already in rather than
  reading it again.
- **The folder tree lists data files too** — everything a source can read, each with
  its type — and clicking one, or a directory row, opens *Register a source* filled
  in from what was clicked: kind, path, file type, an id, and the project registry,
  since it came out of the open folder. A directory still folds away by its arrow.
- **Tick which types the tree shows.** The button in the *Folder* head names them —
  `all`, `sql`, `3 types` — and lists what the folder holds, with a count each. The
  choice outlives the session, and it is the hidden types that are stored, so a
  type the folder gains later shows up rather than being missing from a list
  written before it existed.
- **A dot where the cross would be** marks a tab with unsaved changes; the cross
  comes back when you point at it.

### Elsewhere

- **A project source's relative path is anchored to the open folder**, which is what
  lets a committed `.alkyon/sources.json` say `./sample-data/csv` and mean the same
  thing on every machine. In the user registry a relative path is refused outright,
  rather than resolving against wherever the server happened to start.
- `python docker/seed/make-sample-files.py` writes a `sample-data/` tree to click
  around in: the same 250 customers and 1 500 orders as CSV, JSON, JSON lines,
  parquet and Excel, a deliberately awkward European CSV, and two workbooks of one
  shape in a directory to see a union.
- Two real workbooks are committed under `tests/fixtures/`, so the Excel path is
  tested against files Excel produced rather than against a writer crate.

### Breaking

- **A folder source now has to declare its file type.** One registered before this
  shows a red dot with a message saying so; add the type and it works again.
- **Table names have changed.** A subdirectory is now one table (`sales`) instead of
  one per file (`sales.orders`), the schema is `public` instead of `main`, and the
  catalogue is the folder's name instead of `memory`. Saved SQL that named the old
  shape has to be updated.

## v0.1.0 — 2026-08-02

First release. Four kinds of source (PostgreSQL, SQL Server, MySQL/MariaDB, and a
folder or file read by DuckDB), federation behind `-- @duckdb` and `-- @import`,
results paged off a query held open and drawn on a canvas, credentials in the OS
keychain, and a PTY starting in the folder you opened. Full notes are on the
[release](https://github.com/RWauman/alkyon/releases/tag/v0.1.0).
