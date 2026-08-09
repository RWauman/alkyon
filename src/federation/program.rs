//! Parsing a federated buffer.
//!
//! A federated buffer declares what it needs, then says what to return — the shape
//! DAX uses, and for the same reason: the declarations are the interesting part and
//! they deserve to be readable.
//!
//! ```text
//! DEFINE
//!     ATTACH pg      = pg-prod/warehouse
//!     IMPORT orders  = mssql-prod/sales AS (
//!         SELECT TOP 1000 order_id, customer_id, unit_price
//!         FROM sales.order_line
//!         WHERE shipped_on >= '2026-01-01'
//!     )
//!     FILES  trips   = taxi/*.parquet
//!     EXCEL  budget  = budgets/2026.xlsx#Forecast
//!
//! EVALUATE
//!     SELECT c.name, sum(o.unit_price) AS total
//!     FROM pg.sales.customer c
//!     JOIN orders o ON o.customer_id = c.id
//!     GROUP BY c.name;
//! ```
//!
//! `EVALUATE` on its own is a federated buffer with nothing declared — DuckDB, and
//! whatever the open folder holds.
//!
//! **This replaced a set of `-- @import` comments.** Those kept the file a valid
//! `.sql` that psql would parse, which was worth something; what it cost was a
//! whole class of unreadability — no highlighting, no completion, and native SQL
//! crammed onto one line. Only the query reaches DuckDB now, preceded by as many
//! blank lines as the declarations occupied, so an error still names the line you
//! are looking at.

use crate::error::{Error, Result};

/// The declarations a `DEFINE` block may hold.
const KINDS: [&str; 4] = ["attach", "import", "files", "excel"];

/// Something made available to the DuckDB session before the query runs.
///
/// Not all of these are materialised, and the difference is the whole cost model:
/// [`Import::Query`] and [`Import::Excel`] bring rows across, [`Import::Files`]
/// hands DuckDB a path, and [`Import::Attach`] hands it a live server to plan
/// against. They share this enum because they share one rule — an alias is claimed
/// once, whatever claims it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Import {
    /// `IMPORT <alias> = <source>[/<database>] AS ( <sql> )`
    ///
    /// The SQL is in that source's own dialect, and nothing rewrites it.
    Query {
        alias: String,
        source: String,
        database: Option<String>,
        sql: String,
    },
    /// `FILES <alias> = <folder source>/<path or glob>`
    ///
    /// The one import that does **not** travel through Alkyon: DuckDB reads the
    /// files itself, so no row is ever converted to text on the way.
    Files {
        alias: String,
        source: String,
        /// Relative to that source's path; `*` and `**` are DuckDB's to expand.
        /// Empty when the source is itself a single file.
        pattern: String,
    },
    /// `EXCEL <alias> = <path>[#<sheet>]`, read in-process rather than by DuckDB.
    Excel {
        alias: String,
        path: String,
        sheet: Option<String>,
    },
    /// `ATTACH <alias> = <source>[/<database>]` — a whole server, planned by DuckDB.
    ///
    /// The alias is a **catalogue**, not a table, so what you write is
    /// `pg.sales.customer` rather than a bare name.
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
    /// What runs in DuckDB: the `EVALUATE` body, preceded by a blank line for
    /// every line the declarations took up, so a reported line number is the one
    /// in the editor.
    pub sql: String,
}

/// Is this buffer meant for DuckDB rather than for a single source?
///
/// True when the first thing in it is `DEFINE` or `EVALUATE`. Only the *first*
/// thing: a `DEFINE` two hundred lines down is a column called define, and a
/// buffer should not change engine because of one.
pub fn is_federated(buffer: &str) -> bool {
    let mut scanner = Scanner::new(buffer);
    scanner.trivia();
    matches!(scanner.peek_word().as_deref(), Some("define") | Some("evaluate"))
}

pub fn parse(buffer: &str) -> Result<Program> {
    let mut scanner = Scanner::new(buffer);
    scanner.trivia();

    let mut imports: Vec<Import> = Vec::new();
    match scanner.peek_word().as_deref() {
        Some("define") => {
            scanner.take_word();
            loop {
                scanner.trivia();
                // Anything that is not a declaration ends the block — including a
                // misspelt keyword and a forgotten EVALUATE, which then get one
                // honest message below rather than a complaint about the alias
                // that a half-parsed declaration would have produced.
                let Some(word) = scanner.peek_word() else { break };
                if !KINDS.contains(&word.as_str()) {
                    break;
                }
                let at = scanner.line();
                let import = scanner.declaration().map_err(|e| annotate(e, at))?;
                if let Some(clash) = imports.iter().find(|i| i.alias() == import.alias()) {
                    return Err(annotate(
                        Error::BadRequest(format!(
                            "`{}` is already declared — each name is claimed once",
                            clash.alias()
                        )),
                        at,
                    ));
                }
                imports.push(import);
            }
        }
        Some("evaluate") => {}
        _ => {
            return Err(Error::BadRequest(
                "a federated buffer starts with DEFINE or EVALUATE".into(),
            ))
        }
    }

    scanner.trivia();
    let at = scanner.line();
    let found = scanner.take_word();
    if found.as_deref() != Some("evaluate") {
        let found = found.unwrap_or_else(|| "the end of the buffer".to_owned());
        return Err(Error::BadRequest(format!(
            "line {at}: expected another declaration or EVALUATE, found `{found}` — \
             EVALUATE says what to return, and a declaration is one of {}",
            KINDS.join(", ").to_uppercase()
        )));
    }

    // Blank lines rather than a stripped prefix: the query keeps the line numbers
    // it has in the editor, so DuckDB's complaints point at the right place.
    let body = &buffer[scanner.at..];
    let skipped = buffer[..scanner.at].matches('\n').count();
    let sql = format!("{}{body}", "\n".repeat(skipped));

    if sql.trim().is_empty() {
        return Err(Error::BadRequest(
            "EVALUATE needs a query after it".to_owned(),
        ));
    }
    Ok(Program { imports, sql })
}

fn annotate(error: Error, line: usize) -> Error {
    match error {
        Error::BadRequest(message) => Error::BadRequest(format!("line {line}: {message}")),
        other => other,
    }
}

/// A reader over the buffer that knows where SQL hides its punctuation.
struct Scanner<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Scanner { text, at: 0 }
    }

    /// The 1-based line the reader is on, for an error that has to point at it.
    fn line(&self) -> usize {
        self.text[..self.at].matches('\n').count() + 1
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    /// Whitespace and comments. A declaration block is code, so it gets commented
    /// like code.
    fn trivia(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();

            if trimmed.starts_with("--") {
                let end = trimmed.find('\n').map_or(trimmed.len(), |index| index + 1);
                self.at += end;
            } else if trimmed.starts_with("/*") {
                let end = trimmed.find("*/").map_or(trimmed.len(), |index| index + 2);
                self.at += end;
            } else {
                return;
            }
        }
    }

    /// Horizontal whitespace only — the kind that does not end a declaration.
    fn spaces(&mut self) {
        let rest = self.rest();
        let trimmed = rest.trim_start_matches([' ', '\t']);
        self.at += rest.len() - trimmed.len();
    }

    /// The next bare word, lowercased, without consuming it.
    fn peek_word(&self) -> Option<String> {
        let word: String = self
            .rest()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!word.is_empty()).then(|| word.to_lowercase())
    }

    fn take_word(&mut self) -> Option<String> {
        let word = self.peek_word()?;
        self.at += word.len();
        Some(word)
    }

    /// Up to the end of the line, trimmed.
    fn rest_of_line(&mut self) -> String {
        let rest = self.rest();
        let end = rest.find('\n').unwrap_or(rest.len());
        let taken = rest[..end].trim().to_owned();
        self.at += end;
        taken
    }

    /// One declaration: `<KIND> <alias> = <target>`, and for `IMPORT` the
    /// parenthesised SQL after `AS`.
    fn declaration(&mut self) -> Result<Import> {
        let Some(kind) = self.take_word() else {
            return Err(Error::BadRequest(format!(
                "expected ATTACH, IMPORT, FILES or EXCEL, found `{}`",
                self.rest().chars().take(20).collect::<String>()
            )));
        };

        self.trivia();
        let alias = self.take_word().unwrap_or_default();
        check_alias(&alias)?;
        self.trivia();
        if !self.rest().starts_with('=') {
            return Err(Error::BadRequest(format!(
                "{kind} {alias}: expected `=` after the name"
            )));
        }
        self.at += 1;
        // Spaces, not trivia: a value lives on the line its name is on. Skipping
        // newlines here let `EXCEL b =` with nothing after it swallow the next
        // line — and report the confusion three lines further down.
        self.spaces();

        match kind.as_str() {
            "attach" => {
                let (source, database) = split_target(&self.rest_of_line(), &alias)?;
                Ok(Import::Attach {
                    alias,
                    source,
                    database,
                })
            }
            "import" => {
                let target = self.until_as(&alias)?;
                let (source, database) = split_target(&target, &alias)?;
                let sql = self.parenthesised(&alias)?;
                if sql.trim().is_empty() {
                    return Err(Error::BadRequest(format!("IMPORT {alias}: no SQL given")));
                }
                Ok(Import::Query {
                    alias,
                    source,
                    database,
                    sql,
                })
            }
            "files" => parse_files(alias, &self.rest_of_line()),
            "excel" => parse_excel(alias, &self.rest_of_line()),
            other => Err(Error::BadRequest(format!(
                "`{other}` is not something that can be declared — \
                 use ATTACH, IMPORT, FILES or EXCEL"
            ))),
        }
    }

    /// Everything up to the keyword `AS`, which introduces the SQL.
    fn until_as(&mut self, alias: &str) -> Result<String> {
        let rest = self.rest();
        let mut at = 0;
        while at < rest.len() {
            // A whole word, so a source called `last` does not end the target.
            if rest[at..].len() >= 2
                && rest[at..at + 2].eq_ignore_ascii_case("as")
                && rest[..at].ends_with(char::is_whitespace)
                && rest[at + 2..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || c == '(')
            {
                let target = rest[..at].trim().to_owned();
                self.at += at + 2;
                return Ok(target);
            }
            at += rest[at..].chars().next().map_or(1, char::len_utf8);
        }
        Err(Error::BadRequest(format!(
            "IMPORT {alias}: expected `AS ( … )` with the SQL to run on the source"
        )))
    }

    /// A balanced `( … )`, stepping over anything SQL might hide a bracket inside.
    ///
    /// This is the whole reason there is a scanner rather than a line splitter:
    /// `WHERE note = 'a)b'` must not end the declaration, and neither must a
    /// `-- )` comment or a `[weird)name]` identifier.
    fn parenthesised(&mut self, alias: &str) -> Result<String> {
        self.trivia();
        if !self.rest().starts_with('(') {
            return Err(Error::BadRequest(format!(
                "IMPORT {alias}: expected `(` after AS"
            )));
        }
        let rest = self.rest();
        let bytes = rest.as_bytes();
        let mut depth = 0usize;
        let mut at = 0usize;

        while at < bytes.len() {
            match bytes[at] {
                b'(' => {
                    depth += 1;
                    at += 1;
                }
                b')' => {
                    depth -= 1;
                    at += 1;
                    if depth == 0 {
                        let sql = rest[1..at - 1].trim().to_owned();
                        self.at += at;
                        return Ok(sql);
                    }
                }
                // A quoted string or identifier: everything to its close is text.
                // `''` inside a string is an escaped quote, which the doubling
                // handles for free — the second one reopens what the first closed.
                quote @ (b'\'' | b'"' | b'`') => {
                    at += 1;
                    while at < bytes.len() && bytes[at] != quote {
                        at += 1;
                    }
                    at += 1;
                }
                // T-SQL's bracket identifier. `]]` is its escape, and the same
                // reopening trick applies.
                b'[' => {
                    at += 1;
                    while at < bytes.len() && bytes[at] != b']' {
                        at += 1;
                    }
                    at += 1;
                }
                b'-' if rest[at..].starts_with("--") => {
                    at += rest[at..].find('\n').unwrap_or(rest.len() - at);
                }
                b'/' if rest[at..].starts_with("/*") => {
                    at += rest[at..].find("*/").map_or(rest.len() - at, |end| end + 2);
                }
                _ => at += 1,
            }
        }
        Err(Error::BadRequest(format!(
            "IMPORT {alias}: the `(` after AS is never closed"
        )))
    }
}

/// `<source>[/<database>]`.
///
/// A source key cannot contain `/` — ids are `[A-Za-z0-9._-]+` and the scope
/// prefix uses `:` — so the last one separates the database.
fn split_target(target: &str, alias: &str) -> Result<(String, Option<String>)> {
    let (source, database) = match target.rsplit_once('/') {
        Some((source, database)) => (source.trim().to_owned(), Some(database.trim().to_owned())),
        None => (target.trim().to_owned(), None),
    };
    if source.is_empty() {
        return Err(Error::BadRequest(format!("{alias}: no source given")));
    }
    Ok((source, database))
}

/// `<folder source>/<path or glob>`
///
/// Split on the **first** `/`, not the last: an id cannot contain one, and the
/// pattern very much can (`2022/*.parquet`). That is the opposite of the rule
/// above, where the last `/` introduces a database.
fn parse_files(alias: String, target: &str) -> Result<Import> {
    let (source, pattern) = match target.split_once('/') {
        Some((source, pattern)) => (source.trim().to_owned(), pattern.trim().to_owned()),
        None => (target.trim().to_owned(), String::new()),
    };
    if source.is_empty() {
        return Err(Error::BadRequest(format!("FILES {alias}: no source given")));
    }
    // The pattern is resolved against the source's own directory, so anything
    // that could climb out of it is refused before DuckDB ever sees it.
    if pattern.starts_with('/') || pattern.starts_with('\\') || pattern.contains("..") {
        return Err(Error::BadRequest(format!(
            "FILES {alias}: `{pattern}` must stay inside the source — no `..`, no absolute path"
        )));
    }
    Ok(Import::Files {
        alias,
        source,
        pattern,
    })
}

/// `<path>[#<sheet>]`
fn parse_excel(alias: String, target: &str) -> Result<Import> {
    if target.is_empty() {
        return Err(Error::BadRequest(format!("EXCEL {alias}: no path given")));
    }
    let (path, sheet) = match target.split_once('#') {
        Some((path, sheet)) => (path.trim().to_owned(), Some(sheet.trim().to_owned())),
        None => (target.to_owned(), None),
    };
    Ok(Import::Excel { alias, path, sheet })
}

/// The alias becomes a DuckDB identifier, so keep it to something that needs no
/// quoting — and refuse anything that could be injected into the DDL.
fn check_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        return Err(Error::BadRequest("a declaration needs a name".to_owned()));
    }
    if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || alias.starts_with(|c: char| c.is_ascii_digit())
    {
        return Err(Error::BadRequest(format!(
            "`{alias}` is not a usable name — letters, digits and underscore, not starting \
             with a digit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_keyword_switches_mode() {
        assert!(is_federated("DEFINE\n  ATTACH pg = pg-dev\nEVALUATE\nselect 1"));
        assert!(is_federated("evaluate select 1"));
        assert!(is_federated("\n\n  -- a note\n  DEFINE\n"));
        assert!(is_federated("/* a note */ EVALUATE select 1"));

        assert!(!is_federated("select 1"));
        // Buried after real SQL: not a mode switch, or a long script would change
        // engine because of a column called `define`.
        assert!(!is_federated("select define from t"));
        assert!(!is_federated("select 1;\nDEFINE\n"));
        assert!(!is_federated(""));
    }

    #[test]
    fn parses_every_kind_of_declaration() {
        let program = parse(
            "DEFINE\n\
             \x20   ATTACH pg     = user:pg-dev/warehouse\n\
             \x20   IMPORT orders = mssql-dev/sales AS ( select top 5 id from sales.order_line )\n\
             \x20   FILES  trips  = taxi/2022/*.parquet\n\
             \x20   EXCEL  budget = budgets/2026.xlsx#Forecast\n\
             EVALUATE\n\
             select 1;",
        )
        .unwrap();

        assert_eq!(
            program.imports,
            [
                Import::Attach {
                    alias: "pg".into(),
                    source: "user:pg-dev".into(),
                    database: Some("warehouse".into()),
                },
                Import::Query {
                    alias: "orders".into(),
                    source: "mssql-dev".into(),
                    database: Some("sales".into()),
                    sql: "select top 5 id from sales.order_line".into(),
                },
                Import::Files {
                    alias: "trips".into(),
                    source: "taxi".into(),
                    pattern: "2022/*.parquet".into(),
                },
                Import::Excel {
                    alias: "budget".into(),
                    path: "budgets/2026.xlsx".into(),
                    sheet: Some("Forecast".into()),
                },
            ]
        );
    }

    /// The query keeps the line numbers it has in the editor, so an error DuckDB
    /// reports points at the line you are looking at.
    #[test]
    fn the_query_keeps_its_line_numbers() {
        let program = parse(
            "DEFINE\n\
             \x20   ATTACH pg = pg-dev\n\
             \n\
             EVALUATE\n\
             select 1;",
        )
        .unwrap();
        // `select 1;` is the buffer's fifth line, and it is the fifth line of what
        // DuckDB is handed: four blank ones stand in for the declarations.
        assert_eq!(program.sql, "\n\n\n\nselect 1;");
        assert_eq!(program.sql.trim(), "select 1;");
        assert_eq!(program.sql.lines().count(), 5);
    }

    /// A buffer with nothing to declare is still a federated one.
    #[test]
    fn evaluate_alone_declares_nothing() {
        let program = parse("EVALUATE\nselect * from 'x.parquet';").unwrap();
        assert!(program.imports.is_empty());
        assert_eq!(program.sql.trim(), "select * from 'x.parquet';");
    }

    /// The reason there is a scanner and not a line splitter.
    #[test]
    fn a_bracket_inside_the_sql_does_not_end_the_declaration() {
        for (sql, why) in [
            ("select 'a)b' as x", "a bracket in a string literal"),
            ("select \"od)d\" from t", "a quoted identifier"),
            ("select [od)d] from t", "a T-SQL bracket identifier"),
            ("select 1 -- )\n, 2", "a line comment"),
            ("select 1 /* ) */, 2", "a block comment"),
            ("select coalesce(a, (b)) from t", "nested brackets"),
            ("select 'it''s )' as x", "a doubled quote inside a string"),
        ] {
            let program = parse(&format!(
                "DEFINE\n  IMPORT x = pg-dev AS ( {sql} )\nEVALUATE\nselect 1;"
            ))
            .unwrap_or_else(|e| panic!("{why}: {e}"));
            let Import::Query { sql: got, .. } = &program.imports[0] else {
                panic!("a query import");
            };
            assert_eq!(got, sql, "{why}");
        }
    }

    /// Native SQL over several lines is the point of the parenthesised form.
    #[test]
    fn the_sql_may_span_lines() {
        let program = parse(
            "DEFINE\n\
             \x20   IMPORT orders = mssql-dev/sales AS (\n\
             \x20       SELECT TOP 1000 order_id, unit_price\n\
             \x20       FROM sales.order_line\n\
             \x20       WHERE shipped_on >= '2026-01-01'\n\
             \x20   )\n\
             EVALUATE\n\
             select * from orders;",
        )
        .unwrap();
        let Import::Query { sql, .. } = &program.imports[0] else {
            panic!("a query import");
        };
        assert!(sql.contains("SELECT TOP 1000"), "{sql}");
        assert!(sql.contains("WHERE shipped_on"), "{sql}");
        assert_eq!(sql.lines().count(), 3);
    }

    /// A declaration block is code, so it gets commented like code.
    #[test]
    fn declarations_can_be_commented() {
        let program = parse(
            "DEFINE\n\
             \x20   -- last quarter only\n\
             \x20   ATTACH pg = pg-dev\n\
             \x20   /* not this one yet\n\
             \x20   ATTACH ms = mssql-dev */\n\
             EVALUATE\n\
             select 1;",
        )
        .unwrap();
        assert_eq!(program.imports.len(), 1);
        assert_eq!(program.imports[0].alias(), "pg");
    }

    #[test]
    fn the_database_is_optional() {
        let program = parse("DEFINE\n  ATTACH pg = pg-dev\nEVALUATE\nselect 1;").unwrap();
        assert_eq!(
            program.imports,
            [Import::Attach {
                alias: "pg".into(),
                source: "pg-dev".into(),
                database: None,
            }]
        );
    }

    /// A scope-qualified key carries a colon of its own, which must survive.
    #[test]
    fn a_scope_qualified_key_is_kept_whole() {
        for key in ["user:pg-dev", "project:warehouse"] {
            let program = parse(&format!(
                "DEFINE\n  IMPORT c = {key}/alkyon_demo AS ( select a::text from t )\nEVALUATE\nselect 1;"
            ))
            .unwrap();
            assert_eq!(
                program.imports,
                [Import::Query {
                    alias: "c".into(),
                    source: key.into(),
                    database: Some("alkyon_demo".into()),
                    // The `::` cast is inside the brackets, so nothing touches it.
                    sql: "select a::text from t".into(),
                }],
                "{key}"
            );
        }
    }

    #[test]
    fn a_files_declaration_cannot_climb_out_of_its_source() {
        for pattern in [
            "../secrets/*.parquet",
            "a/../../b.csv",
            "/etc/passwd",
            "\\\\host\\share",
        ] {
            let buffer =
                format!("DEFINE\n  FILES x = data/{pattern}\nEVALUATE\nselect 1;");
            assert!(
                parse(&buffer).is_err(),
                "`{pattern}` should have been refused"
            );
        }
    }

    #[test]
    fn bad_declarations_say_which_line() {
        for (buffer, expected) in [
            ("DEFINE\n\n  NONSENSE x = y\nEVALUATE\nselect 1;", "found `nonsense`"),
            ("DEFINE\n  ATTACH = pg-dev\nEVALUATE\nselect 1;", "needs a name"),
            ("DEFINE\n  ATTACH pg pg-dev\nEVALUATE\nselect 1;", "expected `=`"),
            ("DEFINE\n  IMPORT c = pg-dev\nEVALUATE\nselect 1;", "expected `AS ("),
            ("DEFINE\n  IMPORT c = pg-dev AS ( \nEVALUATE\nselect 1;", "never closed"),
            ("DEFINE\n  IMPORT c = pg-dev AS ( )\nEVALUATE\nselect 1;", "no SQL"),
            ("DEFINE\n  ATTACH pg = pg-dev\nselect 1;", "found `select`"),
            ("DEFINE\n  EXCEL b =\nEVALUATE\nselect 1;", "no path"),
            ("EVALUATE\n", "needs a query"),
            ("select 1", "starts with DEFINE or EVALUATE"),
        ] {
            let error = parse(buffer).expect_err(buffer).to_string();
            assert!(error.contains(expected), "{buffer} gave {error}");
        }
    }

    #[test]
    fn a_name_cannot_smuggle_sql() {
        for alias in ["c; drop table t", "1c", "\"c\"", "c-d", ""] {
            let buffer = format!("DEFINE\n  ATTACH {alias} = pg-dev\nEVALUATE\nselect 1;");
            assert!(
                parse(&buffer).is_err(),
                "`{alias}` should have been refused"
            );
        }
        assert!(parse("DEFINE\n  ATTACH c_2 = pg-dev\nEVALUATE\nselect 1;").is_ok());
    }

    #[test]
    fn a_name_is_claimed_once() {
        let error = parse(
            "DEFINE\n\
             \x20   ATTACH c = pg-dev\n\
             \x20   IMPORT c = mssql-dev AS ( select 1 )\n\
             EVALUATE\n\
             select 1;",
        )
        .expect_err("clash")
        .to_string();
        assert!(error.contains("already declared"), "{error}");
    }

    /// Case is not the point: SQL people shout their keywords, or do not.
    #[test]
    fn keywords_are_case_insensitive() {
        let program = parse(
            "define\n  attach pg = pg-dev\n  import o = ms-dev as ( select 1 )\nevaluate\nselect 1;",
        )
        .unwrap();
        assert_eq!(program.imports.len(), 2);
    }
}
