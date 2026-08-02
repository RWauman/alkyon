# Alkyon — user guide

The short version of everything that matters. For what Alkyon is and why, see the
[README](README.md).

---

## Start it

```sh
cargo run          # http://127.0.0.1:8787
```

That is the whole thing: one binary serving an API and the UI that consumes it.

## Add a source

Click **+** in the *Sources* pane. Alkyon connects **before** it saves anything, so
a wrong password fails in the dialogue rather than at your first query. **Test**
tries the connection without registering it.

| Engine | Dialect | Default port |
|---|---|---|
| PostgreSQL | PL/pgSQL | 5432 |
| SQL Server | T-SQL | 1433 |
| MySQL / MariaDB | MySQL | 3306 |
| **Folder or file** | DuckDB | — |

- **Database** is optional. Left empty you get `postgres`, `master` or
  `information_schema`.
- **Named instance** (SQL Server, on-prem): fill it in and leave **Port** empty.
- **Entra ID**: paste a token from
  `az account get-access-token --resource https://database.windows.net/`.
- **MySQL** has no schema layer — a schema *is* a database. The explorer shows the
  database twice for that reason, and identifiers are quoted with backticks,
  because `"orders"` in MySQL is the *text* `orders`, not the table.

The password goes to the **OS keychain** (Credential Manager, Keychain, libsecret).
Only the host, port, database and username are written to disk. Nothing that comes
back out of the API ever contains a credential.

Sources live in one of two registries, and both are listed with a badge saying
which:

| Registry | Where it lives |
|---|---|
| **user** | your platform config directory — the default |
| **project** | `.alkyon/sources.json` in the open folder — committable, holds no credential |

Ids only have to be unique *within* a registry, so a personal `warehouse` and a
project's `warehouse` can coexist. Where a name could mean either, write
`user:warehouse` or `project:warehouse` — the error tells you when it matters.

A green dot next to each source says whether it answered. It is checked when the
list loads and on **↻**, not on a timer.

## A folder, or one file, as a source

Pick **Folder or file** and give a path on the machine running alkyon; `~` works.
There is no host, no port and no credential — just the path.

Every data file underneath becomes a **table**, named after itself, and every
subdirectory becomes a **schema**:

```text
C:\data\exports\
  customers.parquet        →  customers
  sales\orders.csv         →  sales.orders
```

```sql
select c.name, sum(o.total)
from customers c join sales.orders o on o.customer_id = c.id
group by c.name;
```

No `read_parquet(...)`, no path literals. It is DuckDB SQL, and the source behaves
like any other: green dot, explorer tree, autocompletion, `Ctrl+K` search over the
files' columns, and `SWAP` to switch to it.

- **Formats**: `.csv`, `.tsv`, `.txt`, `.parquet`, `.json`, `.ndjson`, `.jsonl` —
  all read in-process, nothing downloaded. **Excel is not one of them**: point a
  file source at an `.xlsx` and it says so. Spreadsheets go through `-- @excel` in
  a DuckDB buffer, below.
- **Point it at one file** and only that file is reachable — not its neighbours in
  the same directory.
- **Confined** to the path you gave, for reading *and* writing. `copy … to` inside
  it works, which is how you export; anywhere else is refused.
- Two files with the same stem both survive — `sales.csv` and `sales.parquet`
  become `sales` and `sales.parquet`.
- Up to 200 files, 6 directories deep, skipping `.git`, `node_modules` and friends.
- **A folder named `2022` is a schema named `2022`, and SQL reads that as a
  number** — so it needs quotes: `select * from "2022".yellow_202212`. Alkyon
  quotes everything it writes for you, and says which folders are affected when a
  query fails to parse. Renaming them would make the tree lie about your disk.
- Because it is an ordinary source, a federated buffer can join it to a database
  table — and `-- @import x = my-folder/*.parquet` does it without copying a row.
  See [importing files without copying them](#importing-files-without-copying-them).

A file DuckDB cannot parse — mixed line endings are the usual culprit — fails with
the reason and the filename, and costs you only that file.

## Run a query

`Ctrl+Enter` (or `F5`). **With text selected, only the selection runs** — the usual
way to run one statement out of a long script.

Your SQL goes to the engine untouched: T-SQL to SQL Server, PL/pgSQL to Postgres.
No translation layer, so DDL, views, procedures and vendor syntax behave exactly as
the server expects.

**Stop** ends a running query. Fixed-scale numbers (`numeric`, `decimal`) travel
as text and keep every digit rather than being rounded through a float.

### The results grid

Drawn on a canvas, so it scrolls smoothly however many rows it holds. Rows are
numbered down the left, and the numbers stay put as you scroll sideways.

- **Click a column header to sort**, again to reverse it. NULLs go last either
  way. Sorting reorders the view, never the rows, so it costs nothing to undo.
- **Drag a column edge to resize.** Widths start out measured against the first
  couple of hundred rows and are clamped, so one long JSON blob cannot push every
  other column off screen.
- `NULL` is spelled out rather than shown as an empty cell, because empty and
  absent are different answers.

### Pages

**Every result arrives one page at a time.** *Rows per page* in the header sets how
big a page is — 50 000 by default — and **Next ›** reads the following one.

The query stays open between pages, positioned where the last one stopped, so the
next page is *read on* rather than fetched again with an `OFFSET`. That matters for
more than speed: a statement with no `ORDER BY` may come back in a different order
on a second run, so an `OFFSET` page could quietly repeat or skip rows.

A page replaces the one before it, which is what keeps the browser's share of a
result the same size whatever the result is. One taxi folder is 39.6 M rows and
**6.16 GB** of JSON: the server streams all of it in 106 seconds with flat memory,
and a browser tab dies around 14 M rows trying to hold it. Pages are why you never
find that out the hard way.

**‹** goes back through the pages you have already seen, straight from memory —
no re-reading, and it cannot disturb the cursor. There is no page *picker*, and
there cannot be: for a CSV or a streamed query, nobody knows how many pages there
are until the last one arrives.

- Going back is bounded. Roughly 250 000 rows of history are kept; past that the
  oldest pages are dropped and **‹** stops where they end. Run the query again to
  start from the first page.
- Reading a page holds a connection open on the source. Running another query, or
  closing the tab, lets it go.
- **Stop** during a long page abandons it and the query with it.
- Sorting applies to the page on screen, and is cleared when you turn to another
  one — carrying it over would silently re-sort a different set of rows under the
  same arrow.
- To work with more than a page, aggregate in SQL, or export with
  `copy (…) to '${folder}/out.parquet' (format parquet)`.

### Peek at a table

**Click a table in the explorer** and its first 100 rows appear, in that source's own
dialect — `TOP 100` on SQL Server, `LIMIT 100` everywhere else. The buffer is left
alone, so it costs you nothing you were writing. Double-click still inserts the name.

### Point the editor somewhere else

```sql
SWAP pg-prod                    -- change source
SWAP pg-prod.warehouse          -- change source and database
SWAP pg-prod.warehouse;         -- …then carry on in the same buffer
SELECT count(*) FROM orders;
```

Completion after `SWAP` offers **your registered sources and nothing else** — the
only words that mean anything there. Left to the SQL hint, `SWAP pg` was answered
with `PG_CONTEXT` and friends, and since the list opens as you type, Enter
accepted one and the directive quietly stopped being a directive.

Handled by the editor; it never reaches a server. Only recognised as the **first
statement** of the buffer — comments and blank lines above it are stepped over and
left where they are, so a `.sql` file with a header still works. It is `SWAP` and not `USE` because `USE` is a reserved
keyword in T-SQL and a real statement in MySQL, DuckDB and ClickHouse — `SWAP` is
free in all of them. That mattered the moment MySQL was added, and it will again.

## Find things

**The explorer** walks source → database → schema → table → column, loading each
level on first expand. Columns show the engine's own type spelling
(`numeric(12,4)`, `nvarchar(50)`), nullability and primary keys. Double-click a
table to insert its quoted, qualified name.

**Autocompletion is complete as soon as a source is selected** — `Ctrl+Space`, or it
pops up as you type. You do not have to browse to a table before its columns are
offered.

What it inserts is always valid SQL, even when the name is not: a folder called
`2022` is listed as `2022.yellow_202212` — the name you would recognise — and
inserted as `"2022".yellow_202212`, which is the one that parses. Same for a table
that collides with a keyword. Names that need nothing are left plain.

**`Ctrl+K` searches every indexed schema at once** — table names, column names *and
column types*, so `numeric` finds every column declared that way. Each hit says
which source and database it came from. Click one to retarget the editor there and
insert the name.

Search only covers what is **indexed**; the footer says how much that is. A database
is indexed when its source becomes active, and **Index all** walks the rest — one
source at a time, so indexing never means hitting every server at once.

## Work with `.sql` files

Tabs are independent buffers with their own undo history, and **each remembers the
source and database it was last pointed at** — switching tabs switches target. One
tab per environment works well with `SWAP`.

| Key | Does |
|---|---|
| `Alt+N` / `Alt+W` | new tab / close tab |
| `Ctrl+O` | open files |
| `Ctrl+S` / `Ctrl+Shift+S` | save / save as |

`Alt+N` rather than `Ctrl+N` because Chrome keeps `Ctrl+N`, `Ctrl+T` and `Ctrl+W`
for itself — a page never sees them.

A leading UTF-8 BOM is stripped on read. SSMS writes one by default, and left in
place it becomes an invisible first character that both engines reject with a
baffling syntax error. Files are written back without one.

## Open a folder

**Open…** in the *Folder* pane takes a path **on the machine running alkyon** — and
that is also where the terminal starts. `~` works.

It has to be a server-side path: a folder picked in the browser gives back a name
and no path, so it could never tell the shell where to begin, and under Docker the
files sit next to the server rather than next to the browser.

Only `.sql` files are listed, grouped by directory, skipping `.git`,
`node_modules`, `target` and friends. The choice is remembered between runs; if the
folder has since gone, Alkyon starts with none open rather than refusing to start.

The file API is confined to that folder and to `.sql`, checked path component by
path component and then confirmed through the filesystem so a symlink cannot point
out of the tree.

## Join across sources — DuckDB

Put `-- @duckdb` in the leading comments and the buffer runs in DuckDB instead of
against one source. Every directive is a **SQL comment**, so the file stays a valid
`.sql` that opens in SSMS or psql without complaint.

```sql
-- @duckdb
-- @import customer = pg-prod/warehouse  : select id, name, credit from sales.customer
-- @import orders   = mssql-prod/sales   : select top 1000 order_id, customer_id, unit_price
                                           from sales.order_line
-- @excel  budget   = budgets/2026.xlsx#Forecast

select c.name, sum(o.unit_price) as total, b.target
from customer c
join orders o on o.customer_id = c.id
left join '${folder}/data/regions.parquet' r on r.customer_id = c.id
left join budget b on b.customer_id = c.id
group by c.name, b.target;
```

Each `@import` is written **in its own source's dialect** — `top` above is T-SQL and
nothing rewrites it. The federated query on top is DuckDB SQL. No translation
anywhere.

```text
-- @import <alias> = <source>[/<database>] : <SQL in that source's dialect>
-- @import <alias> = <folder source>/<path or glob>      -- no `:` — see below
-- @excel  <alias> = <path>[#<sheet>]
```

### Importing files without copying them

Drop the `:` and the SQL, and the import becomes a **path**:

```sql
-- @duckdb
-- @import trips = taxi/*.parquet
select count(*) as rides, round(sum(total_amount)) as revenue from trips;
```

The missing `:` is the whole distinction: there is no SQL to run on a folder. The
pattern is relative to the source's own path, and `*` / `**` are DuckDB's to
expand — so `taxi/2022/*.parquet` and `taxi/**/*.csv` both work, and a dozen files
become one table.

**This is the one import that does not travel through Alkyon.** A normal `@import`
pulls every row into this process and turns each cell into text; a path import is
a *view* over the files, so DuckDB reads only the columns your query touches. On
39.7 M rows across twelve parquet files: **270 ms**, and no row cap, because no row
is ever copied.

It works on folder and file sources only — anything else has SQL to run, and says
so. The pattern cannot leave the source: `..` and absolute paths are refused before
a path is built, and the sandbox refuses whatever slips past.

`@import` takes **any** registered source, folder sources included — so a parquet
file joins a production table without either side knowing about the other:

```sql
-- @duckdb
-- @import live  = pg-prod/warehouse : select id, name from sales.customer
-- @import bench = exports           : select id, target from benchmarks
select l.name, b.target from live l join bench b on b.id = l.id;
```

**Exporting.** Straight DuckDB:

```sql
copy (select * from customer) to '${folder}/data/customers.parquet' (format parquet);
```

**`${folder}` is how every file is addressed**, for reading and writing alike.
Alkyon expands it before the SQL reaches DuckDB. A bare relative path will not work
— that is a consequence of the sandbox, not an oversight: with file access disabled
the permission check happens on the path as written, before any search path is
consulted.

### What to expect

- **The rows travel through Alkyon** — for `@import … : <sql>` and `@excel`, not for
  a path import. No predicate pushdown, so a single one is capped at 1,000,000 rows
  (`ALKYON_IMPORT_MAX_ROWS`). Exceeding it is an **error**, never a silent
  truncation. Narrow the import, or point at the files by path instead.
- **Formats**: CSV, Parquet and JSON, all built in — nothing is ever downloaded.
  Excel is read in-process, with each column's type inferred from its cells.
  **Delta and Iceberg are not available**; they would require fetching an extension.
- **Decimals** land as `DECIMAL(38,9)`. Result-set metadata carries no scale, so one
  had to be chosen; cast explicitly in the import if you need more.
- **Sandbox**: each federated session can reach the open folder and nothing else, is
  refused any extension install, and is then frozen so a query cannot undo it. With
  no folder open it gets no file access at all. Treat it as defence in depth, not a
  guarantee.

## The terminal

A real PTY — the **⚙** picker lists the shells actually found on the machine, and it
starts in the open folder. This is the seam an LLM agent drives.

Alkyon has no authentication by design, so on a non-loopback bind that endpoint
would be an unauthenticated remote shell. It is therefore **refused unless the
listener is loopback**, or `ALKYON_TERMINAL=always` says otherwise.

## Editing keys

| Key | Does |
|---|---|
| `Ctrl+Enter` / `F5` | run — the selection if there is one, else the buffer |
| `Ctrl+K` | search schemas |
| `Ctrl+Space` | complete tables and columns |
| `Ctrl+F` / `Ctrl+H` / `Ctrl+Shift+H` | find / replace / replace all |
| `Ctrl+G` / `F3` / `Shift+F3` | next / next / previous match |
| `Ctrl+Shift+L` | select every occurrence of the selection and edit them together |
| `Ctrl+D` | add the next occurrence to the selection |
| `Ctrl+/` | toggle comment |
| `Alt+G` | go to line |
| `Alt+N` / `Alt+W` | new tab / close tab |
| `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S` | open / save / save as |

The theme button cycles Auto → Light → Dark and remembers your choice.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `ALKYON_BIND` | `127.0.0.1:8787` | address to listen on |
| `ALKYON_CONFIG_DIR` | platform config dir | where `sources.json` lives |
| `ALKYON_VAULT` | `keyring` | `memory` keeps credentials out of the keychain |
| `ALKYON_SOURCES` | — | JSON file of sources to *import* at startup |
| `ALKYON_SHELL` | PowerShell / `$SHELL` | what the terminal spawns |
| `ALKYON_TERMINAL` | loopback only | `always` to expose the terminal elsewhere |
| `ALKYON_PAGE_ROWS` | `50000` | rows in one page of a result |
| `ALKYON_IMPORT_MAX_ROWS` | `1000000` | cap on one federated `@import` |
| `ALKYON_LOG` | `alkyon=info` | `tracing` filter |

`ALKYON_CONFIG_DIR` is what makes a container or a portable install work — the
directory next to an installed `.exe` is usually not writable.

`ALKYON_SOURCES` is an **import**, not a config file: point it at a JSON array of
sources *with* credentials and the secrets move into the keychain, after which you
can delete the file. In a container, where there is no keychain, pair it with
`ALKYON_VAULT=memory`.

## HTTP API

The UI is only a client; everything is reachable directly.

| Method | Route | |
|---|---|---|
| GET | `/health` | status, version, vault, whether the terminal is available |
| GET / POST | `/sources` | list (no credentials) / register (connects first); a folder source posts `{"kind":"files","path":…,"auth":{"method":"none"}}` |
| DELETE | `/sources/{key}` | also deletes the keychain entry |
| POST | `/connection-test` | try credentials without registering |
| GET | `/sources/{key}/status` | reachable now? always 200; the answer is in the body |
| GET | `/sources/{key}/databases` | |
| GET | `/sources/{key}/tables?db=` | tables and views |
| GET | `/sources/{key}/columns?db=&schema=&table=` | types, nullability, keys, defaults |
| GET | `/sources/{key}/schema?db=&refresh=` | the whole schema in one round trip |
| GET | `/search?q=&limit=` | across every indexed schema |
| GET / PUT / DELETE | `/workspace` | the open folder and its `.sql` files |
| GET / PUT | `/workspace/file?path=` | read / write, confined to the folder |
| GET | `/shells` | shells found on the machine |
| WS | `/ws/query` | `{source_id, database?, sql, page_size?}` → `columns` / `rows` / `affected` / `end` (carrying `page` and `more`). `{"type":"more"}` reads the next page off the query still running; `{"type":"cancel"}` drops it. Closing the socket releases the connection. |
| WS | `/ws/terminal?shell=` | PTY; binary frames are bytes, text frames are control |
