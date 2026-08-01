//! Cached schema snapshots, and the search that reads across them.
//!
//! One snapshot per `(source, database)`. It is what feeds autocompletion the
//! moment a source is selected, and what the search bar looks through — the same
//! data serving both is the reason they were built together.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::model::TableSchema;

/// Search stops here. A hundred hits is already more than anyone reads, and it
/// keeps the response small on a schema with thousands of columns.
pub const MAX_HITS: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub source: String,
    pub database: String,
    pub tables: Vec<TableSchema>,
}

impl Snapshot {
    pub fn column_count(&self) -> usize {
        self.tables.iter().map(|table| table.columns.len()).sum()
    }
}

/// What a hit is: a table, or a column of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HitKind {
    Table,
    View,
    Column,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub kind: HitKind,
    pub source: String,
    pub database: String,
    pub schema: String,
    pub table: String,
    /// Absent for a table hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The engine's spelling of the type, for a column hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_primary_key: bool,
    /// Lower is better. Only used for ordering.
    pub rank: u8,
}

#[derive(Default)]
pub struct SchemaCache {
    snapshots: RwLock<HashMap<(String, String), Arc<Snapshot>>>,
}

impl SchemaCache {
    pub async fn get(&self, source: &str, database: &str) -> Option<Arc<Snapshot>> {
        self.snapshots
            .read()
            .await
            .get(&(source.to_owned(), database.to_owned()))
            .cloned()
    }

    pub async fn put(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let key = (snapshot.source.clone(), snapshot.database.clone());
        let snapshot = Arc::new(snapshot);
        self.snapshots
            .write()
            .await
            .insert(key, Arc::clone(&snapshot));
        snapshot
    }

    /// Forget everything about `source` — used when it is removed or re-registered,
    /// so a stale schema cannot outlive the connection it describes.
    pub async fn forget(&self, source: &str) {
        self.snapshots.write().await.retain(|(s, _), _| s != source);
    }

    /// What is indexed, for the UI to say how much of the search is covered.
    pub async fn indexed(&self) -> Vec<(String, String, usize)> {
        let mut listed: Vec<(String, String, usize)> = self
            .snapshots
            .read()
            .await
            .values()
            .map(|snapshot| {
                (
                    snapshot.source.clone(),
                    snapshot.database.clone(),
                    snapshot.column_count(),
                )
            })
            .collect();
        listed.sort();
        listed
    }

    /// Case-insensitive substring search over every cached snapshot.
    ///
    /// Ranked so that what you probably meant comes first: exact names, then
    /// prefixes, then anything containing the needle; tables ahead of columns at
    /// equal quality.
    pub async fn search(&self, needle: &str, limit: usize) -> Vec<Hit> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for snapshot in self.snapshots.read().await.values() {
            for table in &snapshot.tables {
                if let Some(rank) = rank(&table.name, &needle) {
                    hits.push(Hit {
                        kind: match table.kind {
                            crate::model::TableKind::View => HitKind::View,
                            crate::model::TableKind::Table => HitKind::Table,
                        },
                        source: snapshot.source.clone(),
                        database: snapshot.database.clone(),
                        schema: table.schema.clone(),
                        table: table.name.clone(),
                        column: None,
                        data_type: None,
                        is_primary_key: false,
                        rank,
                    });
                }

                for column in &table.columns {
                    // A column matches on its name or on its type, so `numeric` or
                    // `uniqueidentifier` finds every column declared that way.
                    let by_name = rank(&column.name, &needle);
                    let by_type = rank(&column.data_type, &needle).map(|r| r.saturating_add(3));
                    let Some(rank) = by_name.or(by_type) else {
                        continue;
                    };
                    hits.push(Hit {
                        kind: HitKind::Column,
                        source: snapshot.source.clone(),
                        database: snapshot.database.clone(),
                        schema: table.schema.clone(),
                        table: table.name.clone(),
                        column: Some(column.name.clone()),
                        data_type: Some(column.data_type.clone()),
                        is_primary_key: column.is_primary_key,
                        rank,
                    });
                }
            }
        }

        hits.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                // Tables before their own columns at the same rank.
                .then_with(|| (a.kind == HitKind::Column).cmp(&(b.kind == HitKind::Column)))
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.schema.cmp(&b.schema))
                .then_with(|| a.table.cmp(&b.table))
                .then_with(|| a.column.cmp(&b.column))
        });
        hits.truncate(limit.min(MAX_HITS));
        hits
    }
}

/// `None` when it does not match at all.
fn rank(haystack: &str, needle: &str) -> Option<u8> {
    let lowered = haystack.to_lowercase();
    if lowered == needle {
        Some(0)
    } else if lowered.starts_with(needle) {
        Some(1)
    } else if lowered.contains(needle) {
        Some(2)
    } else {
        None
    }
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
            is_primary_key: pk,
            default: None,
        }
    }

    async fn cache() -> SchemaCache {
        let cache = SchemaCache::default();
        cache
            .put(Snapshot {
                source: "user:pg".into(),
                database: "warehouse".into(),
                tables: vec![
                    TableSchema {
                        schema: "sales".into(),
                        name: "customer".into(),
                        kind: TableKind::Table,
                        columns: vec![
                            column("id", "integer", true),
                            column("customer_name", "varchar(120)", false),
                        ],
                    },
                    TableSchema {
                        schema: "sales".into(),
                        name: "customer_archive".into(),
                        kind: TableKind::View,
                        columns: vec![column("credit", "numeric(18,2)", false)],
                    },
                ],
            })
            .await;
        cache
            .put(Snapshot {
                source: "user:mssql".into(),
                database: "reporting".into(),
                tables: vec![TableSchema {
                    schema: "dbo".into(),
                    name: "invoice".into(),
                    kind: TableKind::Table,
                    columns: vec![column("customer_id", "int", false)],
                }],
            })
            .await;
        cache
    }

    #[tokio::test]
    async fn searches_across_every_cached_source() {
        let hits = cache().await.search("customer", 50).await;
        let seen: Vec<(HitKind, &str, &str)> = hits
            .iter()
            .map(|hit| {
                (
                    hit.kind,
                    hit.source.as_str(),
                    hit.column.as_deref().unwrap_or(hit.table.as_str()),
                )
            })
            .collect();

        assert_eq!(
            seen,
            [
                // Exact table name first.
                (HitKind::Table, "user:pg", "customer"),
                // Then prefixes: the view, then the two columns.
                (HitKind::View, "user:pg", "customer_archive"),
                (HitKind::Column, "user:mssql", "customer_id"),
                (HitKind::Column, "user:pg", "customer_name"),
            ]
        );
    }

    #[tokio::test]
    async fn a_type_name_finds_the_columns_declared_with_it() {
        let hits = cache().await.search("numeric", 50).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column.as_deref(), Some("credit"));
        assert_eq!(hits[0].data_type.as_deref(), Some("numeric(18,2)"));
    }

    #[tokio::test]
    async fn a_primary_key_is_flagged() {
        let hits = cache().await.search("id", 50).await;
        let pk = hits
            .iter()
            .find(|hit| hit.table == "customer" && hit.column.as_deref() == Some("id"))
            .expect("sales.customer.id");
        assert!(pk.is_primary_key);
    }

    #[tokio::test]
    async fn nothing_matches_nothing() {
        let cache = cache().await;
        assert!(cache.search("zzz", 50).await.is_empty());
        assert!(cache.search("   ", 50).await.is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_source_drops_its_snapshots() {
        let cache = cache().await;
        assert_eq!(cache.indexed().await.len(), 2);

        cache.forget("user:pg").await;
        let indexed = cache.indexed().await;
        assert_eq!(
            indexed,
            [("user:mssql".to_owned(), "reporting".to_owned(), 1)]
        );
        assert!(cache
            .search("customer", 50)
            .await
            .iter()
            .all(|hit| hit.source == "user:mssql"));
    }
}
