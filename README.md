<h1 align="center">
  <img src="./logo/alkyon-wordmark.svg" alt="" width="512"><br>
</h1>

<p align="center">
  A portable data workbench: real dialect SQL, optional federation,<br>
  and a terminal your LLM agent can drive.
</p>

**A portable data workbench: real dialect SQL, optional federation, and a terminal your
LLM agent can drive.**

ἀλκυών — the Greek word for the kingfisher, the bird that dives through opaque water and
comes back with the catch. English borrowed it as *halcyon*, along with an H that ancient
scribes added by mistake. Pronounced *AL-kee-on*.

---

## What it is

Alkyon is a single Rust binary. It exposes an HTTP + WebSocket API and serves a static web
UI that consumes it. That is the entire architecture — the desktop installer and the Docker
image are two ways of shipping the same executable, not two codebases.

It connects to **SQL Server** (on-prem, Azure SQL, Microsoft Fabric) and **PostgreSQL**, and
gives you a schema explorer, a multi-dialect SQL editor, a streaming result grid and an
integrated terminal, in roughly ten megabytes.

## Why

Heavyweight database IDEs are built for teams and for every engine on earth. Alkyon is built
for one engineer with two or three sources, who needs to write vendor-specific SQL, join a
spreadsheet against a production table, and hand the tedious parts to an agent — without a
JVM, a workspace concept, or a licence server.

## Two ways to query

**Native.** Pure dialect per source: T-SQL to SQL Server, PL/pgSQL to Postgres. No
translation layer and no lowest common denominator, so DDL, views, stored procedures and
vendor-specific syntax all behave exactly as the server expects. Nothing sits between your
text and the engine.

**Federated.** An optional DuckDB source that attaches Excel files, CSVs and your registered
databases so you can join across them. Enabled per source — if you never turn it on, it is
never in the way.

## Agent-native

The built-in terminal is not a convenience feature, it is the point. Alkyon carries a
Git-versioned folder of Markdown **skills and agents** — a query reviewer, a login and user
provisioner, a procedure scaffolder — injected into the terminal's context. Point Claude Code
or any other CLI agent at it and it inherits your conventions along with your connections.

## Also

- Credentials live in the OS keychain (Windows Credential Manager, Keychain, libsecret),
  never in a config file
- Results stream over WebSocket in batches, so a large `SELECT` renders as it arrives
- Ships as a Tauri installer and as a Docker image

## Not in v1

No in-grid data editing, no migration management, no MySQL or Oracle connectors (the
`Connector` trait is ready for them), no multi-user auth. This is a personal tool and it
is early.