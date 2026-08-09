# Alkyon — user guide

The short version of everything that matters. For what Alkyon is and why, see the
[README](README.md).

---

## Start it

```sh
cargo run          # http://127.0.0.1:8787
```

That is the whole thing: one binary serving an API and the UI that consumes it.

Alkyon opens on a **start screen**, not on an empty query. The first thing to
settle is usually *where* you are working, so it offers that: open a folder, pick
one you opened before, add a source, or start a query anyway.

| | |
|---|---|
| **Open a folder…** | a path on this machine; the `.sql` tree and the terminal follow it |
| **Recent folders** | the last eight, most recent first; ones that have gone are not offered |
| **New query** | `Alt+N`, or the **+** in the tab bar |
| **Add a source…** | the same dialogue as **+** in the *Sources* pane |

Closing the last tab brings the start screen back rather than conjuring an empty
one. Opening a folder does not open a tab — it just puts you somewhere.

## Add a source

Click **+** in the *Sources* pane. Alkyon connects **before** it saves anything, so
a wrong password fails in the dialogue rather than at your first query. **Test**
tries the connection without registering it.

| Engine | Dialect | Default port |
|---|---|---|
| PostgreSQL | PL/pgSQL | 5432 |
| SQL Server | T-SQL | 1433 |
| **Microsoft Fabric SQL endpoint** — via DuckDB | T-SQL — see [below](#microsoft-fabric-sql-endpoint--via-duckdb) | 1433 |
| MySQL / MariaDB | MySQL | 3306 |
| **MongoDB** | DuckDB — see [below](#mongodb-queried-in-sql) | 27017 |
| **Folder of data files** | DuckDB | — |
| **One data file** | DuckDB | — |
| **Azure storage** — Blob, ADLS Gen2, OneLake | DuckDB | — |

- **Database** is optional. Left empty you get `postgres`, `master` or
  `information_schema`.
- **A database that is not there** reads as a login failure on SQL Server — its
  error 4060 says *Cannot open database "x" requested by the login. The login
  failed.* Alkyon rewrites that one, because the server genuinely cannot tell
  "no such database" from "you may not open it", and the tail of the sentence is
  what everyone reads.
- **Named instance** is for on-prem SQL Server only — a bare name like
  `SQLEXPRESS`, resolved by the SQL Browser service on UDP 1434. Fill it in and
  leave **Port** empty. **Never put a hostname here**: no cloud endpoint runs a
  SQL Browser, so the connection times out against a host that answers perfectly
  well. Alkyon refuses a hostname in that field and says where it belongs.
- **A Fabric SQL analytics endpoint does not connect as *SQL Server*.** Pick
  **Microsoft Fabric SQL endpoint — via DuckDB** instead: same host, same sign-in,
  a different TDS implementation, and still T-SQL. See
  [What does not work](#what-does-not-work-yet) for why the ordinary one stops, and
  what the other one gives up.
- **Microsoft Entra — sign in** opens your browser and takes the sign-in from
  there; see [Sign in to Azure](#sign-in-to-azure). **Entra ID access token** is
  still there for a token pasted from
  `az account get-access-token --resource https://database.windows.net/`, which is
  good for the hour Azure gives it.
- **Entra, integrated and pasted tokens are SQL Server only.** The PostgreSQL and
  MySQL connectors take a password and nothing else, so the dialogue no longer
  offers a method those engines could only refuse.
- **MongoDB** takes a login and password, checked against `admin` — which is
  where a root user lives. A user created inside another database has to be given
  there. Left empty, **Database** is `admin`, which is not where anyone's data is.
- **MySQL** has no schema layer — a schema *is* a database. The explorer shows the
  database twice for that reason, and identifiers are quoted with backticks,
  because `"orders"` in MySQL is the *text* `orders`, not the table.

The password goes to the **OS keychain** (Credential Manager, Keychain, libsecret).
Only the host, port, database and username are written to disk. Nothing that comes
back out of the API ever contains a credential.

A secret too long for one entry is split across several — Windows Credential
Manager caps a credential at 2 560 bytes, which is 1 280 characters rather than
the 2 560 its error message names, and an Entra refresh token is longer. They appear as `alkyon / <id>#1`, `#2` beside the source's own
entry; deleting the source removes them all.

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

### Editing one

**✎** on a source row reopens the dialogue on it — to rename it, point it at
another host or database, move it between registries, or give it the password that
changed this morning.

**Leave the password box empty to keep the one in the vault.** That is not a
convenience: the password never comes back out of the API, so an empty box is the
only thing a reopened form can honestly mean by *unchanged*. Type in it and the new
one replaces it. The same goes for a pasted token, and for a sign-in — an Entra
source keeps its refresh token unless you sign in again in the dialogue.

Changing the *method* is the one case where nothing can be kept: a refresh token is
not a password, so alkyon says so rather than guessing.

An edit **connects before it writes**, exactly as adding one does, so an edit that
would break the source fails in the dialogue and leaves the old one working. A
rename onto a name already taken is a conflict, not a silent overwrite.

## A folder, or one file, as a source

Two separate kinds — **Folder of data files** and **One data file** — because they
need different things said about them. Give a path on the machine running alkyon;
`~` works. There is no host, no port and no credential.

The kind is a promise about the path, so pointing a folder source at a file (or the
other way round) is refused rather than quietly doing the other thing.

**The file type comes first**, before the path — a subdirectory's files are read as
one table, and files of two formats cannot be. There is no "work it out per file".

**A file at the root is a table**, named after itself. **A subdirectory is one
table** over every file of that type inside it — unioned, with a `source_file`
column saying which file each row came from, so a folder of monthly exports is one
table and not twelve. Everything lives in the schema `public`, as psql, under a
catalogue named after the folder:

```text
C:\data\exports\            type: Parquet     exports          ← the catalogue
  customers.parquet        →  public.customers
  regions.parquet          →  public.regions
  sales\2023.parquet    ┐
  sales\2024.parquet    ┴  →  public.sales    (+ source_file = '2023' | '2024')
```

```sql
select c.name, sum(o.total)
from customers c join sales o on o.customer_id = c.id
group by c.name;

select source_file, count(*) from sales group by source_file;
```

No `read_parquet(...)`, no path literals. It is DuckDB SQL, and the source behaves
like any other: green dot, explorer tree, autocompletion, `Ctrl+K` search over the
files' columns, and `TARGET` to switch to it.

**The files in one subdirectory must have exactly the same columns.** This is rigid
on purpose: one that does not match is an error naming the file, not a best-effort
reshaping that quietly drops a column.

**Leaving files out.** The dialogue's **Leave out** list is filled from the folder
itself once the path and the type are in — tick a file and the source stops reading
it. Nothing else about the folder changes; a subdirectory left with no files simply
stops being a table.

### A Delta table is a directory

**Delta is the one format that is not a file.** A table is a directory holding a
`_delta_log`, and the log — not the directory listing — says which parquet inside
it are live and what the columns are. So a Delta source finds its tables by the
log: every directory holding one is a table named after itself, and what is inside
belongs to the log rather than to alkyon.

That matters for correctness, not neatness. Unioning the parquet in a Delta
directory returns rows that were deleted and rows that were replaced. Reading the
log returns the table.

It works the same in a folder on this machine and in
[Azure storage](#azure-storage-as-a-source) — a lakehouse's `Tables/` is exactly
this shape:

```text
lake/                       type: Delta table    lake
  sales/_delta_log/…     ┐
  sales/part-0000.parquet ┴ →  public.sales
  returns/_delta_log/…      →  public.returns
```

Reading one needs DuckDB's `delta` extension, which is fetched once from
`extensions.duckdb.org` the first time a Delta source is opened — the same
arrangement as the `azure` extension, and for the same reason. A local Delta
source keeps its confinement intact: the extension is loaded *before* external
access is shut, and both still hold afterwards — a `delta_scan` runs, and a file
outside the granted directory is still refused.

There is no **Leave out** list for Delta and no CSV options: the log describes the
table, so there is nothing to tick or to sniff.

### Formats

| Format | Extensions | Read by |
|---|---|---|
| CSV / delimited | `.csv` `.tsv` `.txt` | DuckDB (core) |
| Parquet | `.parquet` | DuckDB (linked in) |
| JSON | `.json` | DuckDB (linked in) |
| JSON lines | `.jsonl` `.ndjson` | DuckDB (linked in) |
| Excel | `.xlsx` `.xlsm` `.xlsb` `.xls` | calamine, in this process |
| Delta table | — a directory, not a file | DuckDB's `delta` extension, fetched once |

None of them needs anything downloaded. Alkyon leans that way throughout —
a single binary, the readers linked in — but it is a preference, not a vow: an
[Azure storage source](#azure-storage-as-a-source) fetches DuckDB's `azure`
extension once, because reading a container is not otherwise possible.

**Choosing the type** does two things: it narrows a folder to that format's own
extensions — say Parquet and a stray `notes.csv` stops being data — and for a *file*
source it overrides the extension outright, which is how `export.dat` gets read as
CSV. Only that type's options are shown; a delimiter means nothing to a parquet
file, and Parquet and JSON have nothing to ask about at all.

**A workbook becomes one table per sheet.** Two sheets in a root `book.xlsx` are
`book_Customers` and `book_Orders`; a single-sheet workbook keeps its own name, and a
subdirectory of workbooks is named for the directory and the sheet. Name a sheet in
the options and it is the only one — which is also how a subdirectory of monthly
workbooks unions that sheet across them. Unlike the other formats a spreadsheet is
*read* rather than scanned, so it is loaded when a query names it — which is why only
the tables you mention cost anything.

### CSV options

For the file a sniffer gets wrong. Every box left empty means "let DuckDB work it
out", and it usually does: it found the `;` and skipped a two-line preamble in
testing without being told. What it cannot guess is a **decimal comma** — `1000,50`
is a perfectly good string — so a European export arrives as text until you say so.

Delimiter, quote, escape, decimal, encoding (`utf-8`, `utf-16`, `latin-1`), null
text, lines to skip, header yes/no, date and timestamp formats, how many rows the
type sniffer reads (`-1` for all of them, the cure for a column that is integers
for 20 000 rows and then a word), all-text, and whether a bad row fails the query or
is skipped.

- **Point it at one file** and only that file is reachable — not its neighbours in
  the same directory. That is a real difference in what the source may touch, not
  just in what it lists.
- **Confined** to the path you gave, for reading *and* writing. `copy … to` inside
  it works, which is how you export; anywhere else is refused.
- Two names that would collide both survive: a second directory called `2022` keeps
  its whole path (`old_2022`), and a `sales/` beside a `sales.csv` becomes `sales_2`.
  Nothing is ever dropped for want of a name.
- Up to 200 files, 6 directories deep, skipping `.git`, `node_modules` and friends.
- **A folder named `2022` is a table named `2022`, and SQL reads that as a
  number** — so it needs quotes: `select * from "2022"`. Alkyon quotes everything it
  writes for you, and says which names are affected when a query fails to parse.
  Renaming them would make the tree lie about your disk.
- **↻ on the source row re-reads that one source**: new files, new tables, and the
  schema behind autocompletion, without re-probing every server in the sidebar. **✕**
  removes it, and asks first.
- Because it is an ordinary source, a federated buffer can join it to a database
  table — and `FILES x = my-folder/*.parquet` does it without copying a row.
  See [importing files without copying them](#importing-files-without-copying-them).

A file DuckDB cannot parse — mixed line endings are the usual culprit — fails with
the reason and the filename, and costs you only its own directory's table.

### Something to try it on

```sh
python docker/seed/make-sample-files.py
```

Writes `sample-data/` at the repository root: the same 250 customers and 1 500
orders as CSV, JSON, JSON lines, Parquet and Excel — customers at the root, a table
of its own, and orders in a `sales/` subdirectory — plus
`excel/monthly/january.xlsx` and `february.xlsx`, two workbooks of one shape in one
directory to see the union and the `source_file` column, and one deliberately
awkward `csv-european/ventes.csv` that is semicolon-delimited, comma-decimal and
Latin-1 with two lines of preamble.

`delta/` holds two Delta tables, and `delta/customers` is deliberately at odds
with its own directory: the log adds one parquet and **removes** another that is
still sitting there. Read as Delta it has 250 rows; unioning the parquet the way a
plain folder source would gives 310. That difference is the fixture's whole
purpose — it is what [reading the log](#a-delta-table-is-a-directory) buys.

`.alkyon/sources.json` already registers them all as **project** sources, so
opening this repository as a folder is enough to see them.
Parquet and Delta need `pyarrow`, Excel needs `openpyxl`; each is skipped with a message
rather than failing the run.

## MongoDB, queried in SQL

**MongoDB has no SQL of its own.** `$sql` exists, but only inside Atlas Data
Federation — a separate paid service, unreachable from a self-hosted deployment
and from a plain Atlas cluster. So alkyon does not translate SQL into aggregation
pipelines and hope: it reads the documents itself and lets **DuckDB** answer, the
same arrangement a spreadsheet already gets.

Each collection is a view over its documents as JSON, which is the part worth
having:

```sql
select address.city, unnest(tags) as tag, count(*)
from customer
group by all
order by 3 desc;
```

**Nesting survives.** DuckDB infers the shapes rather than being handed a
flattened table, so a sub-document is a `STRUCT` and an array is a `LIST`:

```text
address    struct(city varchar, postcode varchar, region struct(code varchar, name varchar))
tags       varchar[]
loyalty    struct(points bigint, since timestamp)
```

Three things follow from documents not being rows, and each is a deliberate
answer rather than an accident:

- **A field absent from a document is `NULL`** on that row — not `false`, not
  zero. "Not shipped" and "not recorded" stay different facts.
- **A field holding several types across a collection becomes `JSON`**, not the
  type of the first document. `json_type(value)` tells you which is which.
- **A `Decimal128` arrives as text.** Money, exactly: turning it into a float to
  make the column numeric would lose cents. Cast it —
  `cast(credit as decimal(18,2))` — and it sums to the cent.

**The columns are a sample.** A collection has no schema, so the tree and
autocompletion are built from the first 200 documents. A field that first appears
in the ten-thousandth is not in them; it is still queryable, it just is not
offered.

**The database is the schema**, as it is for MySQL — so `customer` and
`"alkyon_demo"."customer"` name the same collection, the second being what the
explorer inserts when you click one.

### What it costs

**Documents arrive before they are filtered.** There is no pushdown: a `where`
clause narrows rows that have already been read. Only the collections your SQL
*names* are read — a query against one does not fetch the others — but that one is
read whole.

The defences are a cap and an escape hatch:

- **200 000 documents** per collection, after which the query is refused rather
  than truncated, because a join quietly missing half its rows is worse than a
  query that failed. Raise it with `ALKYON_MONGO_MAX_DOCS`.
- **`IMPORT` for the times the server should do the work**, written as a real
  aggregation pipeline — nothing is translated, and only the result travels:

  ```text
  EVALUATE
      IMPORT top = mongo-dev/alkyon_demo AS ( [{"$group": {"_id": "$sku", "n": {"$sum": 1}}}] )
  select * from top order by n desc limit 10;
  ```

### Something to try it on

```sh
docker compose -f docker/compose.dev.yml up -d mongo
```

Port 57017, login `alkyon` / `alkyon-dev`, database `alkyon_demo`, and a seed that
is document-shaped on purpose: nested addresses that go deeper on some documents,
a `loyalty` field on every third one, `Decimal128` money, and an `awkward`
collection where one field is in turn an integer, a string, a double, a document
and an array.

## Sign in to Azure

**Microsoft Entra — sign in** is the authentication method for Azure SQL, and the
only one for an Azure storage source. Press **Sign in…**: your browser opens at
Microsoft's page, and when you come back the dialogue names the account. Then
**Connect and save** as usual.

**Device code** is the same sign-in for when the browser is not on the machine
running alkyon — under Docker, or with `ALKYON_BIND` pointed elsewhere. It shows a
code to type at `microsoft.com/devicelogin` from any device, and the sign-in lands
back in the dialogue when you finish.

**What is stored is the refresh token**, in the OS keychain like any other
credential. An access token is minted from it when a connection opens and kept in
memory until it expires, so a source keeps working past the hour Azure gives an
access token — which is the whole difference from pasting one in. Entra rotates
refresh tokens, and the new one replaces the old in the keychain.

**Tenant** and **Application id** are for tenants that need them. Left empty they
are `organizations` and Azure CLI's own application id — a public client with
`http://localhost` registered and pre-consented everywhere, which is what makes
signing in work before anyone has registered anything. A tenant that refuses
unapproved clients needs its own registration: a **public client** with the
redirect URI `http://localhost`, and delegated permissions for whichever of Azure
SQL and Azure Storage you use.

The page never holds a token. The sign-in finishes inside alkyon, which hands the
dialogue a one-use *ticket*; the tokens go from the server's own memory to the
keychain.

**When a sign-in expires** — revoked, or invalidated by a password change — the
source says so and asks you to sign in again rather than failing as a connection
error.

## Azure storage as a source

One kind for three names: a **blob container**, an **ADLS Gen2 filesystem** and a
**Fabric OneLake** workspace are the same API at three hostnames.

| Field | What goes in it |
|---|---|
| **Account** | `contoso`, `contoso.dfs.core.windows.net`, or `onelake.dfs.fabric.microsoft.com`. A blob hostname is accepted and read as the DFS one — same data, and only that endpoint answers JSON |
| **Container and folder** | the container or filesystem first, then how far in: `sales/exports/2026`, or `Workspace/Lakehouse.Lakehouse/Files/exports` for OneLake |
| **File type** | as for a folder source, and for the same reason: a subdirectory's files are read as one table, and files of two formats cannot be |

**Or paste the whole thing in either box.** Fabric's *Copy ABFS path* and the
portal's endpoint field give a URL, and both spellings are taken apart for you:

```text
abfss://<container>@<account>.dfs.core.windows.net/<folder>
https://<account>.dfs.core.windows.net/<container>/<folder>
```

The container moves from one side of the `@` to the front of the path between the
two, which is exactly the sort of thing not worth doing by hand.

Once connected it **is** a folder source: same catalogue, same `public` schema, a
file at the root is a table, a subdirectory is one table over its files with a
`source_file` column, and it federates with `IMPORT` like anything else.

### It reads where the data lies

DuckDB's `azure` extension opens an `abfss://` URL directly, so **nothing is
copied to this machine**. A parquet's schema is a range request for its footer, a
`where` clause is pushed down to the scan, and a container of a thousand files
costs a listing rather than a download. Alkyon lists the container once when the
connection opens — that is what the tables are — and every read after that is
DuckDB's.

**The extension is fetched once.** It is not something alkyon can link in, so the
first time you register an Azure source DuckDB installs it from
`extensions.duckdb.org` into its own cache (`~/.duckdb`), about two megabytes,
signed, and version-matched to the DuckDB inside alkyon. After that there is
nothing to fetch. This is the one place alkyon reaches the network for something
other than your data.

### What the session may touch

A folder source runs with external access **off** and one directory as the
exception. An Azure session cannot: an extension and a network read both need
external access. So it is confined the other way round — the **local filesystem
is shut**:

```text
read a local file            → File system LocalFileSystem has been disabled
reopen the local filesystem  → the configuration has been locked
```

Which makes it *tighter* than a folder source's session, not looser: that one can
read a directory, this one can read nothing on this machine at all. The account is
reached through a secret scoped to it, built from the sign-in's own token — so a
session can read the one account it was opened for.

### Limits

- **Excel is not read over Azure storage.** Alkyon reads spreadsheets with
  calamine, in its own process and from a local file, rather than through DuckDB.
  Every other format is read where it lies. The dialogue greys the option out.
- **5 000 files** per source become tables. Nothing is downloaded, so this is not
  about bandwidth — it is what keeps the tree, and a snapshot that reads one
  header per table, usable. Past it the extra files are left out and the log says
  how many.
- **What has been run against a real account**: a Fabric OneLake workspace, end
  to end — the Entra sign-in, the extension installing itself, a listing of 336
  files, parquet queries, and a Delta table read through its log. A plain blob or
  ADLS Gen2 account uses the same code and the same API; it has not had the same
  run.
- **Permissions**: reading needs the **Storage Blob Data Reader** role on the
  account or container. Owning the storage account is not the same thing — that
  grants management, not data — and it is the usual reason for a surprising 403.

## What does not work yet

Written down because finding it out twice is worse than reading it once.

### An INTERVAL column

The DuckDB crate cannot represent one — mapping its Arrow type panics inside the
driver — so a query selecting an `INTERVAL` fails with an error naming it. The
session survives, which is the part that matters, and `cast(gap as varchar)` reads
fine. Not ours to fix, and pinned by a test so that it stays a contained error
rather than becoming a crash.

### Fabric SQL endpoints and warehouses

**A Fabric SQL analytics endpoint or warehouse cannot be connected to.**
Everything up to the last step works: the Entra sign-in, the token, the first
login — which Fabric *accepts*, then answers with a routing token naming the node
that holds the warehouse. It is the login on that node that fails.

The routed name is `cluster.pbidedicated.windows.net\WORKSPACE-dw`, and its two
halves do different jobs: the host is what the socket and the certificate are
for, the whole name is what the login packet has to carry, or the server does not
know which warehouse is meant. `tiberius`, the TDS driver alkyon uses, takes one
name for both, so it cannot send them. Rather than send half of it and get the
connection closed without explanation, alkyon refuses the redirect and says why.

Ruled out along the way, so that nobody spends the afternoon on them again: the
capacity being paused, a transient fault, the token's audience, the application
the token was issued to, the tenant, the TLS server name, the FEDAUTH encoding,
and the `FEDAUTHINFO` round trip. A patched `tiberius` that sends both names was
tried, and the server's answer did not change by a character — so the missing
name is *necessary* (without it the connection is closed silently) but not
*sufficient*, and what else the node wants is not yet known. Finding out means
reading what a working client sends, which means putting a TDS proxy in front of
SSMS.

**Azure SQL is unaffected**: its redirects name a host and a port and no
instance, so the two names agree and the redirect is followed normally.

**What to use instead**: the *Microsoft Fabric SQL endpoint* source below, which is
the same endpoint reached by another road — explorer included, T-SQL included. Or
an [Azure storage source](#azure-storage-as-a-source), which reads the same
lakehouse over OneLake and answers DuckDB SQL.

### Microsoft Fabric SQL endpoint — via DuckDB

A second kind of SQL Server source, and it exists for exactly one reason: it does
not use `tiberius`, so it does not hit the wall above. DuckDB's community `mssql`
extension speaks its own TDS and takes the **same Entra sign-in** — sign in as
usual, and the bearer token alkyon already mints is handed over.

**It is not a DuckDB source wearing a SQL Server label.** The extension's
`mssql_scan` runs a query on the server verbatim, so what you type is what the
server gets:

```sql
SELECT TOP 10 name, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM sales.customer;
SELECT @@VERSION;
```

`top`, `sys.*`, window functions, `@@VERSION` — all of it. The explorer's tree is
built from the same T-SQL the ordinary SQL Server source uses, so the two describe
one server identically: same schemas, same views marked as views, same composite
primary keys, same `decimal(12,4)`. A test asserts that equality column by column
against a real server.

What it gives up, and it is worth knowing before you pick it:

- **It reads.** `mssql_scan` binds a result set, and the connection is read-only
  besides, so `insert`, `update` and DDL are refused with a sentence saying so. For
  a SQL *analytics endpoint*, which is read-only anyway, that costs nothing; for a
  warehouse you mean to write to, use an ordinary SQL Server source.
- **One result set per run.** A batch of several statements gives back the first.
- **About a second to open**, spent in the extension's catalogue round trip, and
  paid by every query. Noticeable in the explorer, not painful.
- **A query cannot be cancelled** once the server has it: the *Cancel* button stops
  alkyon waiting, not the server working.
- **Third-party code**, and encryption without certificate validation — both
  covered under [`ATTACH`](#sql-server-and-the-community-extension), and both apply
  here. A source set to *Require* is refused rather than quietly weakened.

It works against any SQL Server, not only Fabric — but if the ordinary source can
connect, prefer it: it writes, it cancels, it opens instantly, and it validates
certificates.

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

#### Filter a column

Hover a header and click the **▾** on its right. Plain text matches anywhere in
the cell, case-insensitively; a leading comparison compares instead:

```text
ledger          contains "ledger"
>= 1000         at least 1000
!= INTERNAL     everything else
< 2025-01-01    before that, as text
```

`> >= < <= = !=` are all accepted, and `<>` means `!=`. On a `numeric` or
`decimal` column the comparison is **numeric** — those arrive as text to keep
their digits, and compared as text 999.99 would sort above 1000.

**Below the box, whatever the column can offer:**

- **A list of values with counts**, most frequent first, for any column with few
  enough distinct values to list. Everything starts ticked, because nothing ticked
  is no restriction. **Untick every value and you get no rows** — that is the
  honest answer. `All` and `None` act on the values *shown*, so they compose with
  the text box: type `be`, press `None`, and only the values containing "be" are
  cleared.
- **Two ends and two sliders** for a numeric column, instead of a list. Ticking
  values one at a time is the wrong shape for a measure — and a measure is usually
  all-distinct anyway, so the list would refuse to appear. The boxes take an exact
  bound, the sliders are how you find one, and the ends cannot cross. `Full range`
  removes the filter. Under them: how many rows carried a number and how many did
  not.
- **Year → month → day**, for a date or timestamp column. Ticking a year selects
  the year; unfolding it and unticking a month leaves the rest of the year behind,
  and the year's box then shows as partial. The levels are built from the ISO text
  the values already arrive as — there is no date parsing anywhere, which is also
  why it works the same for a `date` and for a `timestamp`.

All of it is built from **every row the page holds**, never a sample: the counts
describe the page, so a sample would make them quietly wrong. What bounds the work
instead is the list itself — past 500 distinct values there is nothing worth showing,
so the scan stops the moment it knows that, and the popover says to use the box
above.

Filters on different columns combine, a filtered column is marked **⌕** in its
header, and the footer says how many rows are left. `Clear` in the popover removes
one; turning the page removes them all.

**NULL satisfies no comparison**, not even `!=` — counting it there would turn
"not 5" into "not 5, plus everything unknown". It is still findable by filtering
for the text `null`, which is what the cell shows.

Like sorting, **a filter applies to the page in memory, not to the whole result**
— see below. To narrow a whole result, put the condition in the SQL.

### Pages

**Every result arrives one page at a time.** *Rows per page* in the header sets how
big a page is — 50 000 by default — and **Next ›** reads the following one.

**No limit** is there too: one page holding the whole result. It is not a special
case, just a page big enough that a second one never comes, and nothing on the
server sizes itself against the number. Use it when you want the whole answer and
know roughly how big that is. The reason it is not the default is measured rather
than cautious — see below.

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
- Sorting and filtering apply to the page on screen, and are cleared when you turn
  to another one — carrying either over would leave the same arrow, or the same
  "12 of 135", describing a different set of rows.
- To work with more than a page, aggregate in SQL, or export with
  `copy (…) to '${folder}/out.parquet' (format parquet)`.

### Peek at a table

**Click a table in the explorer** and its first 10 000 rows appear in a **preview tab**
— italic in the tab bar, reused for the next table you click, so working through a
schema leaves you with one tab and not thirty. Double-click still inserts the name.

The preview lives in its own tab because **a result belongs to the tab that asked
for it**. Looking at a table used to throw away whatever you had just run; now your
tab keeps its query *and* its result, and you get both back by switching to it.

One consequence worth knowing: each tab holding a result also holds the connection
its pages are read from. Closing the tab lets it go.

### Point the editor somewhere else

```sql
TARGET pg-prod                    -- change source
TARGET pg-prod.warehouse          -- change source and database
TARGET pg-prod.warehouse;         -- …then carry on in the same buffer
SELECT count(*) FROM orders;
```

The source is written **bare** here — no quotes. This line never reaches an
engine, so there is no SQL to quote for, and a quote is not part of a name.
(A qualified name in a query is the opposite case; see just below.)

### Or name the source in the query

```sql
select * from "sales_db".warehouse.public.customer
--          └─ source ──┘└─ db ──┘└ sent as written ┘
```

The first part is the source, the second is the database, and **everything after
that goes to the engine exactly as you typed it** — that part is the engine's
business, not alkyon's. The editor retargets, runs, and stays there.

**Double-quote the source name, always** — `"pg".…`, `"mssql".…`, not `pg.…`.
Alkyon reads that first part and removes it before anything is sent, so the
quotes cost nothing and are never seen by the engine — not even by MySQL, where
`"x"` would otherwise be the *text* `x`. Quoting is the only spelling that works
for every id, since a name with a `-` in it is not a bare SQL identifier at all;
making it the habit means never having to think about which case you are in.

This does not replace `TARGET`. `TARGET` says where the *editor* points and it
stays pointed; a qualified name says where *one statement* goes.

**How many parts, by kind of source.** A folder or file source has no databases to
choose between — DuckDB gives it one catalogue, shown under the folder's own name —
so nothing sits between the source and the name. Everything else has databases, so
the second part is one.

| source | write | goes to |
|---|---|---|
| PostgreSQL | `"pg".warehouse.sales.customer` | db `warehouse`, then `sales.customer` |
| PostgreSQL, default schema | `"pg".warehouse.customer` | db `warehouse`, then `customer` |
| SQL Server | `"mssql".alkyon_demo.sales.customer` | db `alkyon_demo`, then `sales.customer` |
| MySQL | `"mysql".sales.customer` | db `sales`, then `customer` — a MySQL schema *is* a database |
| MongoDB | `"mongo".alkyon_demo.customer` | db `alkyon_demo`, then the collection — the database is the schema, as MySQL |
| folder, subdirectory | `"taxi-data"."2022"` | the folder, then the `2022` directory's table |
| folder, at the root | `"taxi-data".zones` | the folder, then `zones.parquet`'s table |

Rules worth knowing:

- Only when the first part names a **registered source**. Without that,
  `alkyon_demo.sales.customer` — perfectly good three-part T-SQL — would be
  hijacked the day someone registers a source called `alkyon_demo`.
- **Quote the source name every time**, whatever it is called:
  `"pg".warehouse.sales.customer`, `"taxi-data"."2022"`. An id with
  a `-` in it has no other spelling, and the rest read the same either way.
- A folder called `2022` is a table called `2022`, which SQL reads as a number,
  so it needs quotes of its own — see
  [folder sources](#a-folder-or-one-file-as-a-source).
- Names inside strings and comments are left alone.
- **A source name spelled almost right is not a source name.** `"taxi_data".…`
  when the source is `taxi-data` retargets nothing: the statement goes wherever the
  editor was pointed and fails there. Nothing can be said in advance —
  `alkyon_demo.sales.customer` is both a plausible typo and ordinary T-SQL — but
  once the engine has refused, the error names the source you probably meant.
- **One statement, one source.** Naming two is an error that says to use
  `DEFINE` and declare each — joining across sources is what federation is
  for, and it cannot be done by pointing somewhere.
- A federated buffer is left alone entirely: there, each declaration names its
  own source and there is no single target to point at.

Completion after `TARGET` offers **your registered sources and nothing else** —
the only words that mean anything there. Left to the SQL hint, `TARGET pg` was
answered with `PG_CONTEXT` and friends, and since the list opens as you type,
Enter accepted one and the directive quietly stopped being a directive.

Handled by the editor; it never reaches a server. Only recognised as the **first
statement** of the buffer — comments and blank lines above it are stepped over and
left where they are, so a `.sql` file with a header still works.

The word is `TARGET` because the alternatives are all taken. `USE` is a reserved
keyword in T-SQL and a real statement in MySQL, DuckDB and ClickHouse. `SOURCE` is
the MySQL client's own include command — `SOURCE file.sql`, written at the start
of a line, exactly where this directive lives — and it reads backwards besides: a
*source* is the thing you registered, so `SOURCE pg-prod` sounds like declaring
one rather than pointing at it. `TARGET` begins no statement in any dialect.
It used to be spelled `SWAP`, and **`SWAP` is still accepted**, so files you saved
before the rename keep working.

### Which one to reach for

Four ways to say where a query goes. They do different jobs, and the differences
matter more than the syntax.

| you want to | use | it changes |
|---|---|---|
| work in one place for a while | the **Source** picker | the editor, until you change it |
| switch mid-file, in the file | `TARGET pg-prod.warehouse` | the editor, from that line on |
| send *one* statement elsewhere | `"pg-prod".warehouse.sales.customer` | the editor, as a side effect |
| **join across sources** | `DEFINE` … `EVALUATE` | nothing — every declaration names its own |

The first three all end with the editor pointed somewhere, and only ever at **one**
source: a statement goes to one engine. The fourth is the only one that reads from
several at once, because that needs a fourth engine — DuckDB — to join them.

Some worked examples, in the order you would meet them:

```sql
-- Look at a table on the source the editor is already on.
select * from sales.customer limit 100;

-- The same, on a source you are not on, without leaving this tab.
select * from "pg-prod".warehouse.sales.customer limit 100;

-- Point the whole file somewhere and stay there. No quotes on this line.
TARGET mysql-prod.orders
select count(*) from order_line;

-- A parquet folder is a source like any other. No database part, and the
-- `2022` directory is one table over every file in it.
select passenger_count, count(*) from "taxi-data"."2022" group by 1;

-- Two sources at once: this needs federation.
EVALUATE
DEFINE
    IMPORT live  = pg-prod/warehouse AS ( select id, name from sales.customer )
    FILES  trips = taxi-data/**/*.parquet
EVALUATE
select l.name, count(*) from live l join trips t on t.customer_id = l.id group by 1;
```

## Find things

**The sidebar is three sections** — *Search*, *Sources*, *Folder*. Click a header to
fold it away; drag the line between two to give one more room, and double-click that
line to hand it back to its content. Which sections are open, and any height you
dragged, are remembered between runs. `Ctrl+K` unfolds *Search* before focusing it,
since a box inside a folded section is no box at all.

By default each section is as tall as what is in it and *Folder* takes the slack, so
one registered source does not leave half the sidebar empty above the folder tree.

**The explorer** walks source → database → schema → table → column, loading each
level on first expand. Columns show the engine's own type spelling
(`numeric(12,4)`, `nvarchar(50)`), nullability and primary keys. Double-click a
table to insert its quoted, qualified name.

**Autocompletion is complete as soon as a source is selected** — `Ctrl+Space`, or it
pops up as you type. You do not have to browse to a table before its columns are
offered.

**Tables and columns are both offered, whatever the clause.** Each suggestion says
where it came from, on the right: a column shows its table, a table shows its
schema in accent, a CTE says `CTE`. The clause only decides which kind is listed
*first* — after `FROM` and `JOIN` that is tables, everywhere else columns — because
the list pops up as you type and Enter accepts whatever is highlighted.

**Columns are scoped to what the statement reads from.** Once the query names a
table, only that table's columns are offered — `from sales.customer` and you get
`credit`, not the other 280 columns of the database. Aliases count, joins add their
tables, and a subquery's `FROM` counts too. While nothing is named yet, everything
is offered, which is the state you are in when you write the select list first.

A column name that exists in several of the tables in scope is one suggestion
saying `4 tables`, not four suggestions. At most 40 columns are offered at once.

**Columns from your own CTEs are offered too.** `with recent as (select id, name
from sales.customer)` makes `recent` completable after `FROM`, `recent.` complete
its columns, and `id` and `name` appear as plain columns. `select *` inside a CTE
resolves against the schema alkyon already knows, and through a chain of CTEs.

This is read from the buffer, not from a server, by something that is deliberately
**not** a SQL parser: it finds `WITH name AS ( … )`, splits a select list on its
top-level commas, and takes the name off each item. Where it cannot tell, it offers
nothing rather than guessing — an expression with no alias, a `t.*`, a `UNION` past
its first branch. A missing suggestion costs a keystroke; a wrong one costs a
debugging session.

**Type the part you know.** `custo` finds `sales.customer` and inserts it whole —
you do not have to remember the schema first. The list shows `customer — sales`,
table before schema, because the table is what you were looking for.

**After `FROM`, the schemas come first**, then the tables. Typing `sal` offers the
`sales` schema and everything in it.

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

**Drag a tab to reorder it.** A line shows where it will land, and dropping past
the last tab moves it to the end.

Tabs are independent buffers with their own undo history, and **each remembers the
source and database it was last pointed at** — switching tabs switches target. One
tab per environment works well with `TARGET`.

**A dot where the cross would be** means unsaved changes. Point at the tab and the
cross comes back — so the one tab you must not lose by accident is never the one
wearing a close button.

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

Listed are the files Alkyon has something to do with — `.sql` to edit, plus every
data file a source could read (`.csv`, `.tsv`, `.txt`, `.parquet`, `.json`,
`.jsonl`, `.ndjson`, `.xlsx` and the other Excel spellings) — grouped by directory,
skipping `.git`, `node_modules`, `target` and friends. The choice is remembered
between runs; if the folder has since gone, Alkyon starts with none open rather
than refusing to start.

**The button between *Open…* and *↻* says which types are on screen** — `all`,
`sql`, `3 types` — and opens a tickable list of what this folder actually holds,
with a count each, plus **All**. What it hides is remembered between runs, and it
is the *hidden* types that are remembered: a type this folder does not hold today,
or one a later alkyon learns to read, arrives visible rather than missing from a
list written before it existed. Nothing is re-read — the filter narrows the listing
already in the browser.

**One click.** A `.sql` file opens in a tab, or goes to the tab it is already in.
A data file, or a directory row, opens *Register a source* filled in from what you
clicked — kind, path, file type and an id — as a **project** source, since it came
out of the open folder. Fold a directory away with its arrow rather than its name,
which is the part that registers it.

**The listing is taken when you look, not watched.** A file created afterwards — by
the terminal, by another editor — is not there yet. **↻** re-lists, and so does
coming back to the tab, which is when it is most likely to be stale. There is no
filesystem watcher on purpose: it would mean a second long-lived channel to the
browser and a per-platform notifier, for something one directory walk answers.

The last eight folders are remembered too, and offered on the start screen. That
list outlives closing a folder — closing one is not forgetting it — and a folder
that is no longer there is simply not offered, rather than dropped: a path on a
drive that happens to be unplugged today should come back tomorrow.

The file API is confined to that folder and to `.sql`, checked path component by
path component and then confirmed through the filesystem so a symlink cannot point
out of the tree.

## Join across sources — DuckDB

A buffer that begins with **`DEFINE`** declares what it needs, then **`EVALUATE`**
says what to return. It runs in DuckDB instead of against one source, and it is the
one place where several engines meet.

```sql
DEFINE
    ATTACH pg      = pg-prod/warehouse
    IMPORT orders  = mssql-prod/sales AS (
        SELECT TOP 1000 order_id, customer_id, unit_price
        FROM sales.order_line
        WHERE shipped_on >= '2026-01-01'
    )
    FILES  regions = exports/regions.parquet
    EXCEL  budget  = budgets/2026.xlsx#Forecast

EVALUATE
    select c.name, sum(o.unit_price) as total, b.target
    from pg.sales.customer c
    join orders o on o.customer_id = c.id
    left join regions r on r.customer_id = c.id
    left join budget b on b.customer_id = c.id
    group by c.name, b.target;
```

Each `IMPORT` is written **in its own source's dialect** — `TOP` above is T-SQL and
nothing rewrites it. The query after `EVALUATE` is DuckDB SQL. No translation
anywhere.

```text
DEFINE
    ATTACH <name> = <source>[/<database>]                  -- PostgreSQL, MySQL, SQL Server
    IMPORT <name> = <source>[/<database>] AS ( <SQL> )     -- in that source's dialect
    FILES  <name> = <folder source>/<path or glob>
    EXCEL  <name> = <path>[#<sheet>]
EVALUATE
    <DuckDB SQL>
```

**`EVALUATE` on its own** is a federated buffer with nothing declared — DuckDB, and
whatever the open folder holds. That is also what the **⌗** button in the toolbar
writes: it wraps the buffer you already have, because what you had *was* the thing
to return.

**`EVALUATE s1`** — a name and nothing else — means `select * from s1`, the way it
does in DAX. Anything with a space or a bracket in it is a query and is left
exactly as written.

A name is claimed once, whichever keyword claims it, and the block is code rather
than commentary: it is highlighted as code, `--` comments work inside it, and after
`ATTACH x =` the completion offers the sources you have registered and nothing else.

**Completion follows the declarations.** Below `EVALUATE`, the names on offer are
the ones the block declared rather than whichever source happens to be selected:

- an **`ATTACH pg = src/db`** brings every table of that database, as
  `pg.schema.table`, with its columns — the snapshot alkyon already caches.
- an **`IMPORT`** brings the columns its query will produce, read off its own
  select list: `select id, name, 'x' as tab` gives `id`, `name`, `tab`. A bare
  `select * from x` falls back to what that table holds. What cannot be named — a
  `*`, an unaliased `count(*)` — is left out rather than guessed.
- the status line says what it found — `2 declared — 4 names`.

### The plan

**ᵊb** in the toolbar, in a federated buffer, shows what DuckDB is going to do.
The word `EXPLAIN` goes in right after `EVALUATE`, so every line below keeps the
number it has in the editor, and the answer takes the result pane — a plan is a
drawing made of box characters, not a table, and a grid cell would ellipsise it.

**Hold Shift and it runs the query**, as `EXPLAIN ANALYZE`: actual rows and a time
per operator instead of estimates. That is the one worth reading, and the only one
that answers *did my filter really reach the server* — the plan of an attached SQL
Server prints no `Filters:` line, so the row count at the scan is the evidence.

**Inside `AS ( … )` it switches sources.** There you are writing that source's own
SQL, so its tables and columns are what is offered — not the aliases the buffer
declares, which the source has never heard of.

> **This replaced a set of `-- @import` comments, and they no longer work.** Those
> kept the file a valid `.sql` that psql would parse, which was worth something;
> what they cost was unreadability — no highlighting, no completion, and native SQL
> crammed onto one line. Only the query reaches DuckDB now, preceded by as many
> blank lines as the declarations occupied, so an error still names the line you are
> looking at.

### `ATTACH` — let DuckDB read the server itself

`IMPORT` runs *your* SQL on the source. `ATTACH` hands DuckDB the live server and
lets its planner write the remote query:

```sql
DEFINE
    ATTACH pg = pg-prod/warehouse
EVALUATE
select name, credit from pg.sales.customer where country = 'BE';
```

The alias is a **catalogue**, not a table, so the name has three parts —
`pg.sales.customer` — or two for MySQL, which has no schema layer. Nothing is read
until the query asks, and there is no cap because nothing is copied.

**Which to reach for.** Not a matter of taste; they push down different things.
Measured against a real PostgreSQL with `pg_debug_show_queries`, here is what the
server actually receives:

| you write | PostgreSQL receives |
|---|---|
| `select name … where id = 7` | `SELECT "id","name" … WHERE "id" = '7'` — pushed |
| `select count(*) …` | `SELECT NULL FROM "customer"` — **every row** |
| `select country, count(*) … group by 1` | `SELECT "country" FROM "customer"` — all of them |
| `customer join order_line where country='BE'` | two `COPY`s; `order_line` comes over **whole** |

So **projections and filters are pushed down; aggregations and joins are not.**

- **`ATTACH`** when you want a slice of a large remote table, or to join across
  engines without writing SQL per engine. Browsing is cheap: only the columns you
  name cross the wire.
- **`IMPORT`** when the remote engine should do the work — a `group by` over a
  hundred million rows, a window function, a hint, anything in a dialect DuckDB
  does not speak. There the server runs your SQL verbatim and only the answer
  travels.

Both in one buffer is normal, and often right:

```sql
DEFINE
    ATTACH pg  = pg-prod/warehouse
    IMPORT agg = mssql-prod/sales AS (
        select customer_id, sum(unit_price) total
        from sales.order_line group by customer_id
    )

EVALUATE
    select p.name, a.total
    from pg.sales.customer p join agg a on a.customer_id = p.id;
```

**Which engines, and whose code.** PostgreSQL and MySQL use DuckDB's own **core**
extensions. SQL Server uses a **community** one — third-party native code, fetched
once and run inside alkyon. That distinction is real and is covered below.

MongoDB is not offered: its community extension is not published for every DuckDB
build, and alkyon reads MongoDB itself anyway — see [MongoDB, queried in
SQL](#mongodb-queried-in-sql). `ATTACH` on one says so and points at `IMPORT`,
which for it is not a workaround but the supported path.

#### SQL Server, and the community extension

This is the interesting one, because it is a way into a place alkyon's own SQL
Server path cannot reach:

```sql
DEFINE
    ATTACH fab = fabric-endpoint
EVALUATE
select top_customers.name from fab.dbo.top_customers;
```

The extension speaks TDS natively and takes an **Entra bearer token**, which is
exactly what the browser sign-in already mints — so a Fabric or Azure SQL endpoint
that the native connector cannot log into is worth trying here. Sign the source in
as usual; `ATTACH` hands the token over. Filters reach the server: measured on the
demo schema, `where country = 'BE'` produced 83 rows at the scan rather than 250.

Three things to know before relying on it.

- **It is not DuckDB's code.** It is fetched from the community repository the first
  time a buffer asks for it, and it runs in this process with everything this
  process can reach. `ALKYON_COMMUNITY_EXTENSIONS=off` restores the older stance —
  nothing but DuckDB's own signed extensions — at the cost of this feature.
- **It encrypts but never validates the certificate.** Measured: no secret or
  connection-string parameter changes that, and a certificate that cannot match the
  host is accepted anyway. So a source set to *Require*, which means *validate*, is
  **refused** rather than quietly given a weaker connection than it asked for. Use
  `IMPORT` for it, or set the source to *Trust certificate* if that is genuinely
  acceptable there.
- **A buffer can open its own connection.** Once the extension is in the session,
  an `ATTACH` written in your SQL reaches the network even though external access is
  off — the core `postgres` extension refuses the same thing. Files stay shut either
  way. It matters because a `.sql` someone sends you could then connect somewhere
  alkyon never approved; a test pins the behaviour so the day it changes, the guide
  gets corrected.

**It is READ_ONLY, with no opt-out.** `IMPORT` cannot write at all, so this would
otherwise be the one path in the program that mutates a production server. An
`insert` against an attachment is refused by DuckDB itself.

**The credential is DuckDB's to use, not alkyon's.** It opens its own connection, so
it needs a login and a password: `integrated`, an Entra sign-in or a bearer token
cannot be handed over, and `ATTACH` says so before anything is tried. Your
encryption choice does carry over — *Require* becomes libpq's `verify-full` and not
its `require`, which encrypts without checking the certificate.

**Attaching costs no confinement.** `ATTACH` needs the network, and the network is
what a federated session normally has shut. The order is what makes it work:
attach first, grant the open folder, *then* shut the door and lock it. Measured: a
remote count, a pushed-down filter and even a self-join all still answer afterwards,
while `read_csv` on an ungranted local file is refused — and so is a second
`ATTACH` of your own choosing, and so is loading any extension the buffer did not
already get. (The last two hold for the core extensions; the SQL Server one is the
exception noted above.)

### Importing files without copying them

Drop the `:` and the SQL, and the import becomes a **path**:

```sql
EVALUATE
DEFINE
    FILES trips = taxi/*.parquet
EVALUATE
select count(*) as rides, round(sum(total_amount)) as revenue from trips;
```

The missing `:` is the whole distinction: there is no SQL to run on a folder. The
pattern is relative to the source's own path, and `*` / `**` are DuckDB's to
expand — so `taxi/2022/*.parquet` and `taxi/**/*.csv` both work, and a dozen files
become one table.

**This is the one import that does not travel through Alkyon.** A normal `IMPORT`
pulls every row into this process and turns each cell into text; a FILES declaration is
a *view* over the files, so DuckDB reads only the columns your query touches. On
39.7 M rows across twelve parquet files: **270 ms**, and no row cap, because no row
is ever copied.

It works on folder and file sources only — anything else has SQL to run, and says
so. The pattern cannot leave the source: `..` and absolute paths are refused before
a path is built, and the sandbox refuses whatever slips past.

`IMPORT` takes **any** registered source, folder sources included — so a parquet
file joins a production table without either side knowing about the other:

```sql
DEFINE
    IMPORT live  = pg-prod/warehouse AS ( select id, name from sales.customer )
    IMPORT bench = exports           AS ( select id, target from benchmarks )

EVALUATE
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

- **The rows travel through Alkyon** — for `IMPORT … AS ( … )` and `EXCEL`, not for
  a FILES declaration or an `ATTACH`. Everything you wrote is pushed down, because the
  server is the one running it; what comes back is the result.
- **There is no row cap.** There used to be one at a million rows, and it was really
  a memory limit wearing a row count: every cell was held as a `String` in this
  process until the import finished. Rows now go into DuckDB as they arrive, and
  DuckDB spills to a temp directory when it runs out of memory, so the bound is
  disk. Set `ALKYON_IMPORT_MAX_ROWS` if you want a ceiling anyway — on a shared
  machine a mistyped import filling a disk is worth guarding against. Exceeding it
  is an **error**, never a silent truncation.
- **Formats**: CSV, Parquet and JSON, all built in — nothing is ever downloaded.
  Excel is read in-process, with each column's type inferred from its cells.
  **Delta and Iceberg are not available**; they would require fetching an extension.
- **Decimals** land as `DECIMAL(38,9)`. Result-set metadata carries no scale, so one
  had to be chosen; cast explicitly in the import if you need more.
- **Sandbox**: each federated session can reach the open folder and nothing else,
  and is then frozen so a query cannot undo it. With no folder open it gets no file
  access at all. Anything the session needs — an extension, an `ATTACH` — is
  fetched *before* the freeze; your SQL is refused all of it, `install` included.
  Treat it as defence in depth, not a guarantee.

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
| `ALKYON_IMPORT_MAX_ROWS` | — | opt-in ceiling on one federated `IMPORT` |
| `ALKYON_COMMUNITY_EXTENSIONS` | on | `off` refuses DuckDB community extensions, and with them `ATTACH` on SQL Server |
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
| GET / POST | `/sources` | list (no credentials) / register (connects first); a folder source posts `{"kind":"folder","path":…,"options":{"format":"csv"},"auth":{"method":"none"}}` |
| GET | `/files?path=&format=` | the data files a folder holds, relative to it — the source dialog's file list |
| POST | `/auth/entra` | begin a sign-in — `{"kind":"ms_sql"}` opens a browser, `"device_code":true` returns a code to type elsewhere. Answers a `ticket` |
| GET | `/auth/entra/{ticket}` | `pending`, `ready` with the account, `failed` with why, or `unknown` once swept. A source then registers with `{"auth":{"method":"entra","ticket":…}}` |
| PUT | `/sources/{key}` | replace it — rename, repoint, new password; connects first. `"keep_secret": true` reuses the credential in the vault |
| DELETE | `/sources/{key}` | also deletes the keychain entry |
| POST | `/connection-test` | try credentials without registering |
| GET | `/sources/{key}/status` | reachable now? always 200; the answer is in the body |
| GET | `/sources/{key}/databases` | |
| GET | `/sources/{key}/tables?db=` | tables and views |
| GET | `/sources/{key}/columns?db=&schema=&table=` | types, nullability, keys, defaults |
| GET | `/sources/{key}/schema?db=&refresh=` | the whole schema in one round trip |
| GET | `/search?q=&limit=` | across every indexed schema |
| GET / PUT / DELETE | `/workspace` | the open folder, its files (`.sql`, and data files with the `format` they would be read as), and the folders opened before (`recent`) |
| GET / PUT | `/workspace/file?path=` | read / write, confined to the folder |
| GET | `/shells` | shells found on the machine |
| WS | `/ws/query` | `{source_id, database?, sql, page_size?}` → `columns` / `rows` / `affected` / `end` (carrying `page` and `more`). `{"type":"more"}` reads the next page off the query still running; `{"type":"cancel"}` drops it. Closing the socket releases the connection. |
| WS | `/ws/terminal?shell=` | PTY; binary frames are bytes, text frames are control |
