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

### MongoDB

- **MongoDB is a source, queried in DuckDB SQL.** It has no SQL of its own —
  `$sql` exists only inside Atlas Data Federation, a separate paid service that a
  self-hosted deployment cannot reach — so alkyon reads the documents and DuckDB
  answers, rather than translating SQL into aggregation pipelines and being wrong
  in the interesting cases.
- **Nesting survives.** Each collection is a view over its documents as JSON, so a
  sub-document is a `STRUCT` and an array is a `LIST`: `select address.city,
  unnest(tags) from customer` works. A field absent from a document is `NULL` on
  that row, and a field holding several types across a collection becomes `JSON`
  rather than the type of whichever document came first.
- **A `Decimal128` arrives as text**, so no cent is lost on the way to a float.
  `cast(credit as decimal(18,2))` sums exactly.
- **Documents arrive before they are filtered** — there is no pushdown. Only the
  collections a query names are read, and a collection past 200 000 documents is
  refused rather than truncated (`ALKYON_MONGO_MAX_DOCS`). `@import` takes a real
  aggregation pipeline for the times the server should do the work.
- The columns in the tree are inferred from the first 200 documents, because a
  collection has no schema. The guide says so plainly.
- **The database is the schema**, as with MySQL, so the name the explorer inserts
  when you click a collection is one the query engine has: `"alkyon_demo"."customer"`
  and a bare `customer` are the same view.
- `docker compose -f docker/compose.dev.yml up -d mongo` brings up MongoDB 8 on
  port 57017 with a deliberately document-shaped seed: nested addresses of
  differing depth, fields absent from some documents, `Decimal128` money, and a
  collection where one field is in turn an integer, a string, a double, a document
  and an array.

### Federation

- **`@attach` — let DuckDB read the server itself.** `-- @attach pg = pg-prod/warehouse`
  makes the whole database a catalogue: `pg.sales.customer`, planned by DuckDB rather
  than pulled through alkyon. Projections and filters are pushed down to the server;
  **aggregations and joins are not**, measured with `pg_debug_show_queries` and
  written into the guide, because that is what decides which directive to reach for.
  `@import` stays the right one whenever the remote engine should do the work.
- **PostgreSQL, MySQL and SQL Server.** The first two through DuckDB's own core
  extensions; SQL Server through a **community** one, which is third-party native
  code fetched on first use and run inside alkyon. `ALKYON_COMMUNITY_EXTENSIONS=off`
  restores the older stance and gives up that engine.
- **A Fabric SQL endpoint is worth trying again.** The `mssql` extension speaks TDS
  itself and takes the Entra bearer token the browser sign-in already mints, so it
  does not go through `tiberius` and does not hit the routing wall documented in the
  guide. Untested against a real endpoint — one `@attach` line will tell.
- **SQL Server refuses *Require* rather than weakening it.** That extension encrypts
  but never validates a certificate — measured: no parameter changes it, and a
  certificate that cannot match the host is accepted anyway. So a source that asked
  for validation is refused, with the two ways forward named.
- **A gap, written down and pinned by a test**: with that extension loaded, an
  `ATTACH` written in your own SQL reaches the network even though external access
  is off. The core extensions refuse the same thing. Files stay shut either way.
- MongoDB is not attachable: its community extension is not published for every
  DuckDB build, and alkyon reads MongoDB itself. The error says so.
- **READ_ONLY with no opt-out**, since it would otherwise be the one path in the
  program that can write to a production server. The credential goes into a DuckDB
  secret rather than an ATTACH string, and your encryption choice carries over —
  *Require* becomes libpq's `verify-full`, not its `require`, which encrypts without
  checking the certificate.
- **Attaching costs no confinement.** Attach, grant the folder, *then* shut external
  access and lock it: a remote count, a pushed-down filter and a self-join all still
  answer, while an ungranted local file and a second `ATTACH` of your own are both
  refused. Pinned by a test.
- **The million-row import cap is gone.** It was a memory limit wearing a row count:
  every cell was held as a `String` in this process until the import finished. Rows
  now stream into DuckDB as they arrive, and DuckDB spills to a temp directory when
  memory runs out, so the bound is disk. `ALKYON_IMPORT_MAX_ROWS` still sets a
  ceiling if you want one; it no longer sets one by default.

### Fixed

- **A MongoDB failure was a 400, not a 502.** A refused login came back as "your
  request was malformed" when the request was fine and the credential was not — the
  other connectors have said 502 for a server's own refusal all along.
- **Three test suites asserted the relational demo seed against MongoDB**, whose seed
  is document-shaped on purpose, so `cargo test` failed for every source once
  `mongo-dev` was in `ALKYON_SOURCES`. They now cover the sources that carry that
  seed and say why MongoDB's own shape is asserted in `tests/mongo.rs` instead.
- **Dates, decimals, structs and lists coming out of a DuckDB source were Rust's
  `Debug` output.** A date read as `Date32(20455)`, a total as `Decimal(Decimal {
  width: 38, scale: 2, value: 41948375 })`, a struct as `Struct(OrderedMap([…]))`.
  It hid because the formats alkyon reads mostly carry dates as text; MongoDB, whose
  documents are full of timestamps and sub-documents, made it the first thing you
  saw. Now: exact digits for a decimal, ISO-8601 for a date or timestamp, and real
  JSON arrays and objects for lists and structs.
- **An `INTERVAL` column is a clean error rather than a panic.** The DuckDB crate
  cannot map its Arrow type and panics; the query worker contains it, so the
  session survives. `cast(gap as varchar)` reads fine, and a test pins both halves.

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
  unioned with `source_file`, `@import`.
- **It reads where the data lies**, through DuckDB's `azure` extension: nothing is
  copied to this machine, a parquet's schema is a range request for its footer,
  and a `where` clause is pushed down. Alkyon lists the container once — that is
  what the tables are — and every read after that is DuckDB's. The first Azure
  source installs the extension from `extensions.duckdb.org`, about two megabytes,
  once; the guide says so where the promise of a self-contained binary is made,
  which is now stated as the preference it always was rather than a vow.
- **An Azure session is confined the other way round.** A folder source runs with
  external access off and one directory allowed; that is impossible here, because
  an extension and a network read both need it. So the **local filesystem is shut**
  instead, and the configuration locked — making the session tighter than a folder
  source's, which can at least read a directory. The account is reached through a
  secret scoped to it, built from the sign-in's own token.
- **A secret too long for one keychain entry is split across several.** Windows
  Credential Manager caps a credential at 2 560 *bytes* — 1 280 UTF-16 characters,
  not the 2 560 its own error message names — and an Entra refresh token is longer,
  so a source that tested fine could not be saved. A
  secret that fits is still written exactly as before — one entry, the password
  itself — so nothing already stored has to move. Parts never outlive the secret
  that needed them, and a part that has gone missing is an error rather than a
  shorter secret: half a token looks exactly like a wrong password.
- **Delta tables are read through their log**, in a local folder and in Azure
  storage alike. A table is a directory holding a `_delta_log`, and the log says
  which parquet are live — so a file that was replaced or deleted stops being
  rows, which unioning the directory could never do. Found by the log rather than
  by an extension, since Delta is the one format that is not a file. It needs
  DuckDB's `delta` extension, fetched once; a local Delta source keeps its
  confinement, because the extension is loaded before external access is shut and
  both still hold afterwards.
- Verified against a real Fabric OneLake workspace: the sign-in, the extension
  installing itself, a listing of 336 files, parquet queries, and a Delta table
  read through its log. A plain blob or ADLS Gen2 account is the same code against
  the same API, and has not had the same run.
- **Excel is not read over Azure storage**: calamine wants a local file, and there
  is no longer one. The dialogue greys the option out and says why.
- **The dialogue offers only the authentication methods an engine can use**, and
  hides the rest rather than greying them: PostgreSQL and MySQL take a password
  and nothing else, so a list of four where three could only fail read as a
  choice nobody had made yet.
- **A routing token is followed.** Azure SQL answers a login by pointing at the
  node that actually holds the database, and that arrived as *Server requested a
  connection to an alternative address*. The routed name is split on its
  backslash, as Microsoft's own Go driver does: the host is dialled on the port
  the token carries, and the instance is never sent to a SQL Browser that is not
  there. One redirect, never a loop.
- **A hostname in *Named instance* is refused with a sentence saying where it
  belongs.** Naming an instance sends the connection to the SQL Browser on UDP
  1434, which no cloud endpoint runs, so pasting a Fabric endpoint there produced
  a browser timeout accusing a host that answers perfectly well.
- **A redirect carrying an instance is refused with the reason.** The routed name
  `host\instance` needs its two halves sent to two different places — the host to
  the socket and the certificate, the whole name to the login — and `tiberius`
  takes one name for both. Saying so beats sending half of it and having the
  connection closed without a word.
- **A whole `abfss://` or `https://` path can be pasted into either box** of an
  Azure storage source, and is taken apart. Fabric's *Copy ABFS path* puts the
  container before the `@` and the portal puts it first in the path; neither is
  worth unpicking by hand.

### What does not work

- **A Fabric SQL endpoint cannot be connected to**, and the guide lists what was
  ruled out so nobody spends the afternoon on it again. Fabric accepts the first
  login and routes it; the login on the node it routes to is refused. A patched
  `tiberius` that sends both names was tried, and the server's answer did not
  change by a character — so the missing name is necessary but not sufficient,
  and the patch was dropped rather than kept for nothing. The same lakehouse is
  readable through an Azure storage source over OneLake.


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
  parquet, Excel and Delta, a deliberately awkward European CSV, and two workbooks
  of one shape in a directory to see a union. The Delta table disagrees with its
  own directory on purpose — its log removes a parquet that is still there, so it
  has 250 rows where a union would give 310, which is exactly what reading the log
  is for.
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
