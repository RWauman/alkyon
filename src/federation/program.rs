//! Parsing the federated preamble.
//!
//! Every directive is written as a **SQL comment**, which is why a federated
//! buffer is still a valid `.sql` file: opened in SSMS or psql it parses, and the
//! directives are simply ignored. It also means the text handed to DuckDB is the
//! buffer verbatim — no stripping, so error line numbers still line up.
//!
//! ```text
//! -- @duckdb
//! -- @import customer = user:pg-dev/alkyon_demo : select id, name from sales.customer
//! -- @import trips    = taxi/*.parquet
//! -- @excel  budget   = budgets/2026.xlsx#Sheet1
//!
//! select c.name, b.target
//! from customer c join budget b on b.id = c.id;
//! ```
//!
//! The second form has no `:` because there is no SQL to run on a folder — and
//! that is also what tells the two apart.

use crate::error::{Error, Result};

/// Something made available to the DuckDB session before the query runs.
///
/// Not all of these are materialised, and the difference is the whole cost model:
/// [`Import::Query`] and [`Import::Excel`] bring rows across, [`Import::Files`]
/// hands DuckDB a path, and [`Import::Attach`] hands it a live server to plan
/// against. They share this enum because they share one rule — an alias is claimed
/// once, whatever claims it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Import {
    /// The result of native SQL run against a registered source.
    Query {
        alias: String,
        /// Source key or bare id, resolved by the registry.
        source: String,
        database: Option<String>,
        /// In that source's own dialect — nothing translates it.
        sql: String,
    },
    /// Files inside a folder source, read by DuckDB itself.
    ///
    /// The one import that does **not** travel through Alkyon: there is no SQL to
    /// run on a folder, so the federated session reads the parquet directly. No
    /// row cap applies, because no row is ever converted to text on the way.
    Files {
        alias: String,
        /// A `files` source. Any other kind has SQL to run and belongs above.
        source: String,
        /// Relative to that source's path; `*` and `**` are DuckDB's to expand.
        /// Empty when the source is itself a single file.
        pattern: String,
    },
    /// A spreadsheet, read in-process rather than by DuckDB.
    Excel {
        alias: String,
        path: String,
        sheet: Option<String>,
    },
    /// A whole server, attached so DuckDB plans the remote read itself.
    ///
    /// The alias is a **catalogue**, not a table, so what you write is
    /// `pg.sales.customer` rather than a bare name. Nothing is read until the
    /// query asks, and what the server receives is DuckDB's SQL — see
    /// [`crate::federation::attach`] for exactly how much of the query gets
    /// pushed down, which is less than one would hope.
    Attach {
        alias: String,
        source: String,
        database: Option<String>,
    },
}

impl Import {
    pub fn alias(&self) -> &str {
        match self {
            Import::Query { alias, .. }
            | Import::Files { alias, .. }
            | Import::Excel { alias, .. }
            | Import::Attach { alias, .. } => alias,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub imports: Vec<Import>,
    /// The buffer, verbatim. Directives are comments, so DuckDB skips them.
    pub sql: String,
}

/// Is this buffer meant for DuckDB rather than a single source?
///
/// Only recognised in the leading run of comments and blank lines: a `-- @duckdb`
/// buried after a hundred lines of SQL would be a nasty surprise.
pub fn is_federated(buffer: &str) -> bool {
    for line in buffer.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix("--") else {
            return false;
        };
        if directive(comment).is_some_and(|(name, _)| name == "duckdb") {
            return true;
        }
    }
    false
}

/// Split `@name rest` out of a comment body. `None` when it is an ordinary
/// comment — which is also how you disable a directive: break the `@`.
fn directive(comment: &str) -> Option<(&str, &str)> {
    let rest = comment.trim_start().strip_prefix('@')?;
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some((&rest[..end], rest[end..].trim()))
}

pub fn parse(buffer: &str) -> Result<Program> {
    let mut imports = Vec::new();

    for (number, line) in buffer.lines().enumerate() {
        let trimmed = line.trim();
        let Some(comment) = trimmed.strip_prefix("--") else {
            continue;
        };
        let Some((name, rest)) = directive(comment) else {
            continue;
        };

        let at = || format!("line {}", number + 1);
        let import = match name {
            "duckdb" => continue,
            "import" => parse_query_import(rest).map_err(|e| annotate(e, &at()))?,
            "attach" => parse_attach(rest).map_err(|e| annotate(e, &at()))?,
            "excel" => parse_excel_import(rest).map_err(|e| annotate(e, &at()))?,
            // An unknown `@word` is just prose in a comment.
            _ => continue,
        };

        if let Some(clash) = imports
            .iter()
            .find(|i: &&Import| i.alias() == import.alias())
        {
            return Err(Error::BadRequest(format!(
                "{}: `{}` is already imported — aliases must be unique",
                at(),
                clash.alias()
            )));
        }
        imports.push(import);
    }

    Ok(Program {
        imports,
        sql: buffer.to_owned(),
    })
}

fn annotate(error: Error, at: &str) -> Error {
    match error {
        Error::BadRequest(message) => Error::BadRequest(format!("{at}: {message}")),
        other => other,
    }
}

/// The scope prefixes a source key may carry. These hold the only colon that can
/// legitimately appear before the SQL separator, so the parser steps over one.
const SCOPES: &[&str] = &["user:", "project:"];

/// `<alias> = <source>[/<database>] : <native sql>`, or, with no `:` at all,
/// `<alias> = <folder-source>[/<path or glob>]`.
fn parse_query_import(rest: &str) -> Result<Import> {
    let (alias, remainder) = split_once_trimmed(rest, '=').ok_or_else(|| {
        Error::BadRequest("@import needs `<alias> = <source>[/<database>] : <sql>`".to_owned())
    })?;
    check_alias(&alias)?;

    // Splitting on the first colon would cut `user:pg-dev` in half. Step past a
    // scope prefix, then the next colon is the one that introduces the SQL — and
    // any `::` cast in that SQL comes after it, so it is never touched.
    let scope = SCOPES
        .iter()
        .find(|scope| remainder.starts_with(**scope))
        .map_or(0, |scope| scope.len());
    let Some(separator) = remainder[scope..].find(':').map(|index| index + scope) else {
        // No SQL to run means the target is files, not a server. Which source
        // kinds that is legal for is the registry's business, not the parser's.
        return parse_files_import(alias, &remainder[..]);
    };

    let target = remainder[..separator].trim().to_owned();
    let sql = remainder[separator + 1..].trim().to_owned();
    if sql.is_empty() {
        return Err(Error::BadRequest(format!("@import {alias}: no SQL given")));
    }

    let (source, database) = split_target(&target, &format!("@import {alias}"))?;
    Ok(Import::Query {
        alias,
        source,
        database,
        sql,
    })
}

/// `<source>[/<database>]`.
///
/// A source key cannot contain `/` — ids are `[A-Za-z0-9._-]+` and the scope
/// prefix uses `:` — so the last one separates the database.
fn split_target(target: &str, what: &str) -> Result<(String, Option<String>)> {
    let (source, database) = match target.rsplit_once('/') {
        Some((source, database)) => (source.trim().to_owned(), Some(database.trim().to_owned())),
        None => (target.trim().to_owned(), None),
    };
    if source.is_empty() {
        return Err(Error::BadRequest(format!("{what}: no source given")));
    }
    Ok((source, database))
}

/// `<alias> = <source>[/<database>]`
///
/// No `:` and no SQL, and that is the point: there is nothing to write in the
/// source's dialect because DuckDB generates the remote query itself.
fn parse_attach(rest: &str) -> Result<Import> {
    let (alias, target) = split_once_trimmed(rest, '=').ok_or_else(|| {
        Error::BadRequest("@attach needs `<alias> = <source>[/<database>]`".to_owned())
    })?;
    check_alias(&alias)?;

    // Someone who has been writing `@import` all morning will type a colon here.
    // Saying why there is no SQL to write is more use than "unexpected character".
    // Past the scope prefix, which carries the one colon that is legitimate.
    let scope = SCOPES
        .iter()
        .find(|scope| target.starts_with(**scope))
        .map_or(0, |scope| scope.len());
    if target[scope..].contains(':') {
        return Err(Error::BadRequest(format!(
            "@attach {alias}: takes no SQL — DuckDB writes the remote query itself. Drop the \
             `:` to attach the whole database, or write `@import {alias} = … : <sql>` to run \
             that SQL on the server instead."
        )));
    }

    let (source, database) = split_target(&target, &format!("@attach {alias}"))?;
    Ok(Import::Attach {
        alias,
        source,
        database,
    })
}

/// `<alias> = <folder-source>[/<path or glob>]`
///
/// Split on the **first** `/`, not the last: an id cannot contain one, and the
/// pattern very much can (`2022/*.parquet`). That is the opposite of the SQL form
/// above, where the last `/` introduces a database.
fn parse_files_import(alias: String, target: &str) -> Result<Import> {
    let (source, pattern) = match target.split_once('/') {
        Some((source, pattern)) => (source.trim().to_owned(), pattern.trim().to_owned()),
        None => (target.trim().to_owned(), String::new()),
    };
    if source.is_empty() {
        return Err(Error::BadRequest(format!(
            "@import {alias}: no source given"
        )));
    }
    // The pattern is resolved against the source's own directory, so anything
    // that could climb out of it is refused before DuckDB ever sees it.
    if pattern.starts_with('/') || pattern.starts_with('\\') || pattern.contains("..") {
        return Err(Error::BadRequest(format!(
            "@import {alias}: `{pattern}` must stay inside the source — no `..`, no absolute path"
        )));
    }
    Ok(Import::Files {
        alias,
        source,
        pattern,
    })
}

/// `<alias> = <path>[#<sheet>]`
fn parse_excel_import(rest: &str) -> Result<Import> {
    let (alias, target) = split_once_trimmed(rest, '=')
        .ok_or_else(|| Error::BadRequest("@excel needs `<alias> = <path>[#<sheet>]`".to_owned()))?;
    check_alias(&alias)?;
    if target.is_empty() {
        return Err(Error::BadRequest(format!("@excel {alias}: no path given")));
    }

    let (path, sheet) = match target.split_once('#') {
        Some((path, sheet)) => (path.trim().to_owned(), Some(sheet.trim().to_owned())),
        None => (target, None),
    };
    Ok(Import::Excel { alias, path, sheet })
}

fn split_once_trimmed(text: &str, separator: char) -> Option<(String, String)> {
    let (left, right) = text.split_once(separator)?;
    Some((left.trim().to_owned(), right.trim().to_owned()))
}

/// The alias becomes a DuckDB identifier, so keep it to something that needs no
/// quoting — and refuse anything that could be injected into the DDL.
fn check_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        return Err(Error::BadRequest("an import needs an alias".to_owned()));
    }
    if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || alias.starts_with(|c: char| c.is_ascii_digit())
    {
        return Err(Error::BadRequest(format!(
            "`{alias}` is not a usable alias — letters, digits and underscore, not starting with a digit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_directive_switches_mode() {
        assert!(is_federated("-- @duckdb\nselect 1"));
        assert!(is_federated("\n\n  --   @duckdb  \nselect 1"));
        assert!(is_federated("-- a note\n-- @duckdb\nselect 1"));

        assert!(!is_federated("select 1"));
        // Buried after real SQL: not a mode switch, or a long script would change
        // engine because of a comment near the bottom.
        assert!(!is_federated("select 1;\n-- @duckdb\nselect 2"));
        // Broken on purpose is how you turn it off.
        assert!(!is_federated("-- x@duckdb\nselect 1"));
        assert!(!is_federated("-- duckdb\nselect 1"));
    }

    #[test]
    fn parses_a_query_import() {
        let program = parse(
            "-- @duckdb\n\
             -- @import customer = user:pg-dev/alkyon_demo : select id, name from sales.customer\n\
             select * from customer;",
        )
        .unwrap();

        assert_eq!(
            program.imports,
            [Import::Query {
                alias: "customer".into(),
                source: "user:pg-dev".into(),
                database: Some("alkyon_demo".into()),
                sql: "select id, name from sales.customer".into(),
            }]
        );
        // The buffer goes to DuckDB verbatim: directives are comments already.
        assert!(program.sql.contains("-- @import"));
    }

    #[test]
    fn the_database_is_optional() {
        let program = parse("-- @import c = pg-dev : select 1").unwrap();
        assert_eq!(
            program.imports,
            [Import::Query {
                alias: "c".into(),
                source: "pg-dev".into(),
                database: None,
                sql: "select 1".into(),
            }]
        );
    }

    /// A scope-qualified key carries a colon of its own. Splitting on the first
    /// colon cut `user:pg-dev` in half, leaving `source: "user"`.
    #[test]
    fn a_scope_qualified_key_is_not_cut_in_half() {
        for (key, expected) in [
            ("user:pg-dev", "user:pg-dev"),
            ("project:warehouse", "project:warehouse"),
        ] {
            let program = parse(&format!(
                "-- @import c = {key}/alkyon_demo : select a::text from t"
            ))
            .unwrap();
            assert_eq!(
                program.imports,
                [Import::Query {
                    alias: "c".into(),
                    source: expected.into(),
                    database: Some("alkyon_demo".into()),
                    // The `::` cast sits after the separator, so it survives.
                    sql: "select a::text from t".into(),
                }],
                "{key}"
            );
        }

        // Without a database, too.
        let program = parse("-- @import c = user:pg-dev : select 1").unwrap();
        assert_eq!(
            program.imports,
            [Import::Query {
                alias: "c".into(),
                source: "user:pg-dev".into(),
                database: None,
                sql: "select 1".into(),
            }]
        );
    }

    #[test]
    fn native_sql_may_contain_colons_and_equals() {
        let program =
            parse("-- @import c = pg-dev : select a::text as x from t where b = 1 and c = 2")
                .unwrap();
        let Import::Query { sql, .. } = &program.imports[0] else {
            panic!("a query import");
        };
        assert_eq!(sql, "select a::text as x from t where b = 1 and c = 2");
    }

    /// No `:` means there is no SQL to run, which means the target is files.
    #[test]
    fn an_import_without_sql_is_a_file_import() {
        let program = parse("-- @import taxi = nytc-parquet-test/*.parquet").unwrap();
        assert_eq!(
            program.imports,
            [Import::Files {
                alias: "taxi".into(),
                source: "nytc-parquet-test".into(),
                pattern: "*.parquet".into(),
            }]
        );
    }

    /// The pattern may contain `/`; the id may not. So the split is on the first
    /// one — the opposite of the SQL form, where the last `/` names a database.
    #[test]
    fn the_pattern_keeps_its_own_slashes() {
        for (target, source, pattern) in [
            ("data/2022/*.parquet", "data", "2022/*.parquet"),
            ("user:data/**/*.csv", "user:data", "**/*.csv"),
            (
                "project:exports/one.parquet",
                "project:exports",
                "one.parquet",
            ),
            // A single-file source has nothing to select inside it.
            ("budget", "budget", ""),
        ] {
            let program = parse(&format!("-- @import x = {target}")).unwrap();
            assert_eq!(
                program.imports,
                [Import::Files {
                    alias: "x".into(),
                    source: source.into(),
                    pattern: pattern.into(),
                }],
                "{target}"
            );
        }
    }

    #[test]
    fn a_file_import_cannot_climb_out_of_its_source() {
        for pattern in [
            "../secrets/*.parquet",
            "a/../../b.csv",
            "/etc/passwd",
            "\\\\host\\share",
        ] {
            let buffer = format!("-- @import x = data/{pattern}");
            assert!(
                parse(&buffer).is_err(),
                "`{pattern}` should have been refused"
            );
        }
    }

    #[test]
    fn parses_an_attach() {
        let program = parse(
            "-- @duckdb\n\
             -- @attach pg = user:pg-dev/warehouse\n\
             select * from pg.sales.customer;",
        )
        .unwrap();
        assert_eq!(
            program.imports,
            [Import::Attach {
                alias: "pg".into(),
                source: "user:pg-dev".into(),
                database: Some("warehouse".into()),
            }]
        );

        // The database is optional here too — the source's own is used.
        let program = parse("-- @attach pg = pg-dev").unwrap();
        assert_eq!(
            program.imports,
            [Import::Attach {
                alias: "pg".into(),
                source: "pg-dev".into(),
                database: None,
            }]
        );
    }

    /// Anyone who has been writing `@import` all morning will reach for the colon.
    /// Saying why there is no SQL to write beats "unexpected character".
    #[test]
    fn an_attach_with_sql_explains_itself() {
        let error = parse("-- @attach pg = pg-dev : select 1")
            .expect_err("no SQL belongs here")
            .to_string();
        assert!(error.contains("takes no SQL"), "{error}");
        assert!(error.contains("@import"), "{error}");

        // But a scope prefix carries a legitimate colon of its own.
        assert!(parse("-- @attach pg = user:pg-dev/warehouse").is_ok());
    }

    /// One alias, one meaning — whichever directive claimed it.
    #[test]
    fn an_attach_clashes_with_an_import_of_the_same_name() {
        let error = parse(
            "-- @attach c = pg-dev\n\
             -- @import c = mssql-dev : select 1",
        )
        .expect_err("clash")
        .to_string();
        assert!(error.contains("already imported"), "{error}");
    }

    #[test]
    fn parses_an_excel_import() {
        let program = parse("-- @excel budget = budgets/2026.xlsx#Forecast").unwrap();
        assert_eq!(
            program.imports,
            [Import::Excel {
                alias: "budget".into(),
                path: "budgets/2026.xlsx".into(),
                sheet: Some("Forecast".into()),
            }]
        );

        let program = parse("-- @excel budget = 2026.xlsx").unwrap();
        assert_eq!(
            program.imports,
            [Import::Excel {
                alias: "budget".into(),
                path: "2026.xlsx".into(),
                sheet: None,
            }]
        );
    }

    #[test]
    fn ordinary_comments_are_left_alone() {
        let program = parse(
            "-- @duckdb\n\
             -- joins the warehouse against last year's budget\n\
             -- TODO: @someone should check the rounding\n\
             select 1;",
        )
        .unwrap();
        assert!(program.imports.is_empty());
    }

    #[test]
    fn bad_directives_say_which_line() {
        for (buffer, expected) in [
            ("-- @duckdb\n-- @import oops", "line 2"),
            ("-- @import c = : select 1", "no source"),
            ("-- @import c = pg-dev :", "no SQL"),
            ("-- @import c = /x.parquet", "no source"),
            ("-- @import c = data/../../etc/passwd", "must stay inside"),
            ("-- @import c = data//absolute", "must stay inside"),
            ("-- @excel b =", "no path"),
        ] {
            let error = parse(buffer).expect_err(buffer).to_string();
            assert!(error.contains(expected), "{buffer} gave {error}");
        }
    }

    #[test]
    fn an_alias_cannot_smuggle_sql() {
        for alias in ["c; drop table t", "c d", "1c", "\"c\"", "c-d", ""] {
            let buffer = format!("-- @import {alias} = pg-dev : select 1");
            assert!(
                parse(&buffer).is_err(),
                "`{alias}` should have been refused"
            );
        }
        assert!(parse("-- @import c_2 = pg-dev : select 1").is_ok());
    }

    #[test]
    fn duplicate_aliases_are_refused() {
        let error = parse(
            "-- @import c = pg-dev : select 1\n\
             -- @import c = mssql-dev : select 2",
        )
        .expect_err("clash")
        .to_string();
        assert!(error.contains("already imported"), "{error}");
    }
}
