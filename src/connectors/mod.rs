pub mod adls;
pub mod files;
pub mod mssql;
pub mod mysql;
pub mod postgres;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::Result;
use crate::model::{ColumnInfo, Dialect, RowBatch, SourceConfig, TableInfo, TableSchema};

#[async_trait]
pub trait Connector: Send + Sync {
    /// Open a connection bound to `config.database`, failing if the server is
    /// unreachable or the credentials are wrong.
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>>;

    fn dialect(&self) -> Dialect;
}

#[async_trait]
pub trait Connection: Send + Sync {
    async fn list_databases(&self) -> Result<Vec<String>>;

    /// Tables and views in `db`, which may be any database on the same server.
    async fn list_tables(&self, db: &str) -> Result<Vec<TableInfo>>;

    async fn list_columns(&self, db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>>;

    /// Every table in `db` with its columns, in a *fixed* number of round trips.
    ///
    /// Assembling this by calling `list_columns` per table would be one query per
    /// table — fine for the demo schema, unusable against a real one.
    async fn snapshot(&self, db: &str) -> Result<Vec<TableSchema>>;

    /// Run `sql` against the database this connection is bound to, streaming the
    /// result. Dropping the returned stream cancels the query.
    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>>;
}
