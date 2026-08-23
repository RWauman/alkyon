//! What the model is told, and what it is not.
//!
//! The whole design of ghost text is here rather than in the request: a
//! suggestion is only as good as the schema it can see, and **a schema is far
//! too big to send.** Estimated on the shape of a real Fabric warehouse — 150
//! tables of 18 columns — spelling the whole thing out is around 17 500 tokens.
//! Per keystroke that is neither fast nor cheap, and it is mostly noise: a
//! statement reads two or three tables.
//!
//! So the prompt is a selection, in two layers:
//!
//! - **The tables the buffer names**, with their columns and types. The editor
//!   has already parsed them for the completion dropdown, so they arrive in the
//!   request rather than being parsed again — which is what keeps a SQL parser
//!   out of this file.
//! - **Every other table, as a bare name.** Around 1 400 tokens for those 150
//!   tables, and it is what lets a suggestion propose a table the buffer has not
//!   mentioned yet — the case a columns-only prompt cannot help with at all.
//!
//! Both layers, and the buffer itself, are cut to a character budget, so the
//! request is bounded whatever the schema and whatever the buffer.
//!
//! **Nothing here reads a row.** The schema and the text being written travel;
//! data does not.

use crate::model::{Dialect, TableSchema};
use crate::schema::Snapshot;

/// Budgets, in characters. Characters and not tokens because counting tokens
/// would mean a round trip before the round trip; dense SQL runs around 3.4
/// characters per token, so these are roughly 2 400 / 1 800 / 1 200 / 300.
const DETAIL_CHARS: usize = 8_000;
const NAMES_CHARS: usize = 6_000;
const PREFIX_CHARS: usize = 4_000;
const SUFFIX_CHARS: usize = 1_000;

/// Where the suggestion goes. Out of band on purpose: no SQL contains it, so it
/// cannot be mistaken for a piece of the buffer.
pub const CURSOR: &str = "<|cursor|>";

pub struct Ask<'a> {
    pub dialect: Dialect,
    pub source: &'a str,
    pub database: &'a str,
    /// `None` when the schema has not been snapshotted yet — the suggestion is
    /// then made from the buffer alone rather than not made at all.
    pub snapshot: Option<&'a Snapshot>,
    /// The tables the buffer references, as the editor read them.
    pub tables: &'a [String],
    pub prefix: &'a str,
    pub suffix: &'a str,
    pub max_tables: usize,
}

fn dialect_name(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::TSql => "T-SQL (Microsoft SQL Server)",
        Dialect::PgSql => "PostgreSQL",
        Dialect::MySql => "MySQL",
        Dialect::DuckDb => "DuckDB SQL",
    }
}

/// A name as SQL lets it be written, reduced to something comparable: quoting
/// gone, case gone. `[Sales].[Customer]` and `sales.customer` are one name.
fn bare(name: &str) -> String {
    name.trim()
        .trim_end_matches(';')
        .chars()
        .filter(|c| !matches!(c, '"' | '[' | ']' | '`'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `wanted` — a reference as written — names this table.
///
/// Generous on purpose. A reference may be bare (`customer`), qualified
/// (`sales.customer`) or three-part (`warehouse.sales.customer`), and offering
/// the columns of a table that turns out not to be the one meant costs a few
/// tokens, while missing the right one costs the whole suggestion.
fn names(table: &TableSchema, wanted: &str) -> bool {
    if wanted == bare(&table.qualified()) {
        return true;
    }
    match wanted.rsplit_once('.') {
        // Qualified: the table part has to match, and whatever is in front of it
        // has to be this table's schema — or end in it, for a three-part name.
        Some((head, tail)) => {
            tail == bare(&table.name)
                && (head == bare(&table.schema)
                    || head.ends_with(&format!(".{}", bare(&table.schema))))
        }
        None => wanted == bare(&table.name),
    }
}

fn spell(table: &TableSchema) -> String {
    let columns: Vec<String> = table
        .columns
        .iter()
        .map(|column| {
            let key = if column.is_primary_key { " PK" } else { "" };
            format!("{} {}{key}", column.name, column.data_type)
        })
        .collect();
    format!("{}({})", table.qualified(), columns.join(", "))
}

/// Keep whole lines until `budget` is spent, and admit how many were dropped.
///
/// Said rather than silently truncated: a list that stops without a word reads
/// as the whole list, and a model told that is a model inventing the rest.
fn within(lines: Vec<String>, budget: usize, what: &str) -> String {
    let mut kept = String::new();
    let mut dropped = 0usize;
    for line in lines {
        if kept.len() + line.len() + 1 > budget {
            dropped += 1;
            continue;
        }
        kept.push_str(&line);
        kept.push('\n');
    }
    if dropped > 0 {
        kept.push_str(&format!("-- and {dropped} more {what}, not listed\n"));
    }
    kept
}

/// The last `budget` characters, cut at a line boundary — the cursor is at the
/// end, so the end is the part that matters, and half a line read as a whole one
/// is worse than a shorter buffer.
fn tail(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let cut = text.len() - budget;
    match text[cut..].find('\n') {
        Some(at) => &text[cut + at + 1..],
        None => &text[cut..],
    }
}

fn head(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    match text[..budget].rfind('\n') {
        Some(at) => &text[..at],
        None => &text[..budget],
    }
}

/// What never changes between two keystrokes.
///
/// Kept apart from [`user`] because it is the half that could be cached: it is
/// byte-identical for every request in a project, whereas the schema slice moves
/// with the statement and the buffer moves with every character. Caching is not
/// worth it at this size — see the `llm` module docs — but the split is what
/// makes it a one-field change when the context grows.
pub fn system(dialect: Dialect) -> String {
    format!(
        "You are the inline completion of a SQL editor. You are given a database schema and a \
         buffer with {CURSOR} marking one position in it.\n\n\
         Reply with the text that goes at {CURSOR} and nothing else: no explanation, no markdown, \
         no code fence, and never a repeat of the text that already follows the cursor. If nothing \
         useful goes there, reply with nothing at all.\n\n\
         Write {}. Use only the tables and columns you were given — never invent a name. Keep it \
         short: finish the identifier, the clause or the line being typed, not the whole query. \
         Follow the buffer's own capitalisation and indentation.",
        dialect_name(dialect)
    )
}

/// The half that moves: the schema slice, then the buffer.
///
/// In that order, and not the other way round. The buffer changes on every
/// keystroke and the slice only when the statement names another table, so the
/// stabler part goes first — which is where a cache breakpoint would have to sit.
pub fn user(ask: &Ask) -> String {
    let mut out = String::new();

    if let Some(snapshot) = ask.snapshot {
        let wanted: Vec<String> = ask.tables.iter().map(|name| bare(name)).collect();
        let (referenced, rest): (Vec<_>, Vec<_>) = snapshot
            .tables
            .iter()
            .partition(|table| wanted.iter().any(|name| names(table, name)));

        // Past `max_tables` a referenced table does not vanish — it falls through
        // to the names below. Dropping it outright is what the first version did,
        // and a statement naming forty tables then arrived describing twelve as
        // though that were all of them: a silent cap, which reads to a model as
        // "these are the tables" and is answered with an invented name.
        let detailed = &referenced[..referenced.len().min(ask.max_tables)];
        let overflow = &referenced[detailed.len()..];

        // Columns for what the statement reads. With nothing named yet there is
        // nothing to narrow by and no columns worth guessing at — the names below
        // are what a `select ` with no `from` can be helped with.
        if !detailed.is_empty() {
            out.push_str(&format!(
                "-- tables this statement reads, in {} / {}\n",
                ask.source, ask.database
            ));
            let spelled = detailed.iter().map(|table| spell(table)).collect();
            out.push_str(&within(spelled, DETAIL_CHARS, "tables"));
            if !overflow.is_empty() {
                out.push_str(&format!(
                    "-- and {} more it reads, listed by name below\n",
                    overflow.len()
                ));
            }
            out.push('\n');
        }

        let listed: Vec<String> = overflow
            .iter()
            .chain(rest.iter())
            .map(|table| table.qualified())
            .collect();
        if !listed.is_empty() {
            out.push_str("-- other tables in this database, names only\n");
            out.push_str(&within(listed, NAMES_CHARS, "tables"));
            out.push('\n');
        }
    }

    out.push_str("-- buffer\n");
    out.push_str(tail(ask.prefix, PREFIX_CHARS));
    out.push_str(CURSOR);
    out.push_str(head(ask.suffix, SUFFIX_CHARS));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, TableKind};

    fn column(name: &str, data_type: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            ordinal: 1,
            data_type: data_type.into(),
            nullable: true,
            default: None,
            is_primary_key: pk,
        }
    }

    fn table(schema: &str, name: &str) -> TableSchema {
        TableSchema {
            schema: schema.into(),
            name: name.into(),
            kind: TableKind::Table,
            columns: vec![
                column("id", "int", true),
                column("label", "nvarchar(50)", false),
            ],
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            source: "user:dw".into(),
            database: "warehouse".into(),
            tables: vec![
                table("sales", "customer"),
                table("sales", "order_line"),
                table("control", "perimeter"),
            ],
        }
    }

    fn ask<'a>(held: &'a Snapshot, tables: &'a [String], prefix: &'a str) -> Ask<'a> {
        Ask {
            dialect: Dialect::TSql,
            source: "user:dw",
            database: "warehouse",
            snapshot: Some(held),
            tables,
            prefix,
            suffix: "",
            max_tables: 12,
        }
    }

    /// The two layers, and the reason there are two: columns for what is read,
    /// names for everything else.
    #[test]
    fn a_referenced_table_brings_its_columns_and_the_rest_only_their_names() {
        let held = snapshot();
        let referenced = ["sales.customer".to_owned()];
        let prompt = user(&ask(&held, &referenced, "select  from sales.customer"));

        assert!(
            prompt.contains("sales.customer(id int PK, label nvarchar(50))"),
            "{prompt}"
        );
        // The others are there to be findable, without their columns.
        assert!(prompt.contains("control.perimeter"), "{prompt}");
        assert!(!prompt.contains("control.perimeter(id"), "{prompt}");
    }

    /// A name is a name however SQL lets you write it. This is what decides
    /// whether the columns of the right table travel.
    #[test]
    fn quoting_case_and_qualification_do_not_hide_a_table() {
        let held = snapshot();
        for written in [
            "sales.customer",
            "Sales.Customer",
            "[sales].[customer]",
            "\"sales\".\"customer\"",
            "customer",
            "warehouse.sales.customer",
        ] {
            let named = [written.to_owned()];
            let prompt = user(&ask(&held, &named, "select 1"));
            assert!(
                prompt.contains("sales.customer(id int"),
                "`{written}` did not find the table: {prompt}"
            );
        }
    }

    /// Naming nothing is the state you are in while typing the select list
    /// first. There is nothing to narrow by, so no columns are guessed at.
    #[test]
    fn a_statement_naming_no_table_gets_names_and_no_columns() {
        let held = snapshot();
        let prompt = user(&ask(&held, &[], "select "));
        assert!(!prompt.contains("(id int"), "{prompt}");
        assert!(prompt.contains("sales.customer"), "{prompt}");
        assert!(prompt.contains("names only"), "{prompt}");
    }

    /// The cursor has to be in there, in the right place, or the model is being
    /// asked to complete the end of the buffer instead of the cursor.
    #[test]
    fn the_buffer_carries_the_cursor_between_its_halves() {
        let held = snapshot();
        let mut it = ask(&held, &[], "select c.");
        it.suffix = "\nfrom sales.customer c";
        let prompt = user(&it);
        assert!(
            prompt.contains(&format!("select c.{CURSOR}\nfrom sales.customer c")),
            "{prompt}"
        );
    }

    /// The bound that makes the request affordable whatever the schema. 400
    /// tables of 20 columns is roughly 50 000 tokens spelled out; this is the
    /// check that none of it can arrive by accident.
    #[test]
    fn a_huge_schema_is_cut_to_the_budget_and_says_so() {
        let tables: Vec<TableSchema> = (0..400)
            .map(|i| {
                let mut held = table("sales", &format!("dim_customer_extended_{i}"));
                held.columns = (0..20)
                    .map(|c| column(&format!("column_{c}"), "decimal(18,2)", false))
                    .collect();
                held
            })
            .collect();
        let held = Snapshot {
            source: "user:dw".into(),
            database: "warehouse".into(),
            tables,
        };
        // Every one of them referenced: the worst case for the detail layer.
        let referenced: Vec<String> = (0..400)
            .map(|i| format!("sales.dim_customer_extended_{i}"))
            .collect();
        let prompt = user(&ask(&held, &referenced, "select 1"));

        assert!(
            prompt.len() < DETAIL_CHARS + NAMES_CHARS + 2_000,
            "{} chars",
            prompt.len()
        );
        // Twelve get their columns; the other 388 are handed down to the names
        // layer rather than dropped, because a table cut for budget must not read
        // as a table that does not exist.
        assert!(prompt.contains("-- and 388 more it reads"), "{prompt}");
        assert!(prompt.contains("dim_customer_extended_50"), "{prompt}");
        // At this size the names layer runs out of budget too — and says so. Both
        // caps are admitted, which is the whole point: what is missing is stated,
        // never implied by absence.
        assert!(prompt.contains("not listed"), "{prompt}");
    }

    /// The same cap, at the size it actually bites: more referenced tables than
    /// `max_tables`, few enough that every name fits.
    #[test]
    fn a_table_cut_for_budget_is_still_named() {
        let held = Snapshot {
            source: "user:dw".into(),
            database: "warehouse".into(),
            tables: (0..20).map(|i| table("sales", &format!("t{i}"))).collect(),
        };
        let referenced: Vec<String> = (0..20).map(|i| format!("sales.t{i}")).collect();
        let mut it = ask(&held, &referenced, "select 1");
        it.max_tables = 3;
        let prompt = user(&it);

        assert!(prompt.contains("sales.t0(id int PK"), "{prompt}");
        assert!(!prompt.contains("sales.t9(id"), "past the cap, no columns");
        assert!(prompt.contains("-- and 17 more it reads"), "{prompt}");
        for i in 0..20 {
            assert!(prompt.contains(&format!("sales.t{i}")), "sales.t{i} vanished");
        }
    }

    /// A long buffer is cut from the *front*, because the cursor is at the back —
    /// and never mid-line.
    #[test]
    fn a_long_buffer_keeps_the_end_and_cuts_whole_lines() {
        let held = snapshot();
        let long = format!("{}select final_line", "-- filler line\n".repeat(1_000));
        let prompt = user(&ask(&held, &[], &long));

        assert!(
            prompt.contains(&format!("select final_line{CURSOR}")),
            "the cursor's own line went missing"
        );
        assert!(
            prompt.len() < PREFIX_CHARS + NAMES_CHARS + 2_000,
            "{} chars",
            prompt.len()
        );
        let buffer = prompt.split("-- buffer\n").nth(1).unwrap();
        assert!(
            buffer.starts_with("-- filler line") || buffer.starts_with("select"),
            "cut mid-line: {:?}",
            &buffer[..40.min(buffer.len())]
        );
    }

    #[test]
    fn no_snapshot_still_makes_a_prompt_out_of_the_buffer() {
        let it = Ask {
            dialect: Dialect::PgSql,
            source: "user:pg",
            database: "postgres",
            snapshot: None,
            tables: &[],
            prefix: "select ",
            suffix: "",
            max_tables: 12,
        };
        assert!(user(&it).contains(&format!("select {CURSOR}")));
        assert!(system(Dialect::PgSql).contains("PostgreSQL"));
    }
}
