//! Azure storage as a source: a blob container, an ADLS Gen2 filesystem, or a
//! Fabric OneLake workspace.
//!
//! **It reads where the data lies.** DuckDB's `azure` extension opens an
//! `abfss://` URL directly, so nothing is copied here: a parquet's footer is a
//! range request, a `where` clause is pushed down, and a folder of a thousand
//! files costs a listing rather than a download.
//!
//! That leaves this file with two jobs and no third. **List** the container over
//! HTTPS, which is what says where the tables are and what they are called; and
//! **name** them with the same rules a folder source uses — a file at the root is
//! a table, a subdirectory is one table over its files. Everything after that is
//! [`FilesConnection`], unchanged, because by then a file is only an address in a
//! literal.
//!
//! The session is confined the other way round from a folder source's. There,
//! external access is off and one directory is the exception. Here it has to stay
//! on — an extension and a network read both need it — so the local filesystem is
//! shut instead, which makes this session able to read the account and *nothing*
//! on this machine. See [`crate::federation::open_duckdb_azure`].

use std::path::Path;

use async_trait::async_trait;

use super::files::FilesConnection;
use super::{Connection, Connector};
use crate::azure::storage::{self, Client, Location};
use crate::error::{Error, Result};
use crate::federation::AzureAccess;
use crate::model::{AuthConfig, Dialect, FileFormat, SourceConfig};

/// As many files as one source will name as tables.
///
/// Nothing is downloaded, so this is not about bandwidth: it is what keeps the
/// explorer tree, and a snapshot that reads one header per table, from becoming
/// something nobody can use. Far above the two hundred a local folder stops at,
/// because a listing is one round trip and a directory walk is not.
const MAX_FILES: usize = 5_000;

pub struct AdlsConnector;

/// Whether this source reads that file, by the format it declared.
fn keeps(path: &str, format: FileFormat) -> bool {
    // A Delta table has no extension to match: what is worth keeping from the
    // listing is its log, because that is what says a directory is a table. The
    // parquet inside are the log's business, not the listing's.
    if format.is_delta() {
        return path.contains("_delta_log/");
    }
    let extension = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    extension.is_some_and(|e| format.extensions().contains(&e.as_str()))
}

/// The part of a listed path below the source's prefix.
///
/// The service answers paths relative to the *filesystem*, prefix included; the
/// tables are named from the prefix down, so that a source pointed at
/// `sales/exports` gives tables named after what is in `exports` and not after
/// `exports` itself.
fn below(prefix: &str, path: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(path.to_owned());
    }
    path.strip_prefix(prefix)
        .map(|rest| rest.trim_start_matches('/').to_owned())
        .filter(|rest| !rest.is_empty())
}

/// The `abfss://` URL DuckDB reads. The container goes before the `@`, which is
/// the ABFS driver's spelling and the one Fabric hands out. `path` is as the
/// listing gave it — relative to the filesystem, prefix included.
fn address(location: &Location, path: &str) -> String {
    format!("abfss://{}@{}/{path}", location.filesystem, location.host)
}

/// The storage account a host names: `contoso.dfs.core.windows.net` is `contoso`,
/// and OneLake is `onelake`. What `CREATE SECRET` wants.
fn account_of(host: &str) -> String {
    host.split('.').next().unwrap_or(host).to_owned()
}

/// What the explorer shows where a server shows a database.
fn catalogue_of(location: &Location) -> String {
    let deepest = location
        .prefix
        .rsplit('/')
        .find(|segment| !segment.is_empty());
    deepest.unwrap_or(&location.filesystem).to_owned()
}

#[async_trait]
impl Connector for AdlsConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let location = storage::locate(&config.host, config.path.as_deref())?;
        // Same rule as a folder source, and for the same reason: files of two
        // formats cannot be one table, so which one this holds is not guessable.
        let format = config.options.format.ok_or_else(|| {
            Error::BadRequest(format!(
                "source `{}` reads a folder in Azure storage and must say which file \
                 type it holds (csv, parquet, json, json_lines or delta).",
                config.id
            ))
        })?;
        if format.is_excel() {
            return Err(Error::Unsupported(
                "a spreadsheet in Azure storage cannot be read yet: alkyon reads Excel with \
                 calamine, in this process and from a local file, rather than with DuckDB. \
                 Every other format is read where it lies."
                    .into(),
            ));
        }

        let AuthConfig::AadToken { token } = &config.auth else {
            return Err(Error::Unsupported(
                "an Azure storage source is reached with a Microsoft Entra sign-in".into(),
            ));
        };

        // One listing, at connect. It is what the tables are, and the only network
        // call this file makes — the reads belong to DuckDB.
        let client = Client::new(location.clone(), token.clone())?;
        let listed = client.list().await?;

        let mut wanted: Vec<(String, String)> = listed
            .into_iter()
            .filter_map(|entry| {
                let relative = below(&location.prefix, &entry.path)?;
                keeps(&relative, format).then(|| (relative, address(&location, &entry.path)))
            })
            .collect();

        if wanted.is_empty() {
            return Err(Error::BadRequest(if format.is_delta() {
                format!(
                    "no Delta table under `{}/{}` — a table is a directory holding a `_delta_log`",
                    location.filesystem, location.prefix
                )
            } else {
                format!(
                    "no {} file under `{}/{}`",
                    format.extensions().join(" or "),
                    location.filesystem,
                    location.prefix
                )
            }));
        }
        if wanted.len() > MAX_FILES {
            let dropped = wanted.len() - MAX_FILES;
            tracing::warn!(
                kept = MAX_FILES,
                dropped,
                "the container holds more files than one source will name as tables"
            );
            wanted.truncate(MAX_FILES);
        }

        let access = AzureAccess {
            account: account_of(&location.host),
            token: token.clone(),
            catalogue: catalogue_of(&location),
        };
        let connection = FilesConnection::in_azure(access, &wanted, format, config.options.clone());

        tracing::info!(
            host = %location.host,
            filesystem = %location.filesystem,
            files = wanted.len(),
            "opened an Azure storage source"
        );
        Ok(Box::new(connection))
    }

    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn onelake() -> Location {
        Location {
            host: "onelake.dfs.fabric.microsoft.com".to_owned(),
            filesystem: "workspace-guid".to_owned(),
            prefix: "lakehouse-guid/Files/exports".to_owned(),
        }
    }

    #[test]
    fn an_address_is_the_url_duckdb_reads() {
        assert_eq!(
            address(&onelake(), "lakehouse-guid/Files/exports/2026/january.parquet"),
            "abfss://workspace-guid@onelake.dfs.fabric.microsoft.com/\
             lakehouse-guid/Files/exports/2026/january.parquet"
        );
    }

    #[test]
    fn the_account_is_what_create_secret_wants() {
        assert_eq!(account_of("contoso.dfs.core.windows.net"), "contoso");
        assert_eq!(account_of("onelake.dfs.fabric.microsoft.com"), "onelake");
        // A host with no dots is already an account name.
        assert_eq!(account_of("contoso"), "contoso");
    }

    #[test]
    fn the_catalogue_is_the_deepest_thing_named() {
        assert_eq!(catalogue_of(&onelake()), "exports");
        // Nothing but a container: the container is the name.
        assert_eq!(
            catalogue_of(&Location {
                prefix: String::new(),
                ..onelake()
            }),
            "workspace-guid"
        );
        // A trailing slash is typing, not a level.
        assert_eq!(
            catalogue_of(&Location {
                prefix: "a/b/".to_owned(),
                ..onelake()
            }),
            "b"
        );
    }

    #[test]
    fn paths_are_rooted_at_the_prefix_not_the_filesystem() {
        assert_eq!(
            below("exports", "exports/2026/a.csv").unwrap(),
            "2026/a.csv"
        );
        // The prefix itself is not a file under it.
        assert_eq!(below("exports", "exports"), None);
        assert_eq!(below("", "a/b.csv").unwrap(), "a/b.csv");
        // Something the listing named that is not under the prefix at all.
        assert_eq!(below("exports", "other/a.csv"), None);
    }

    #[test]
    fn only_the_declared_formats_files_are_read() {
        assert!(keeps("a/b.csv", FileFormat::Csv));
        assert!(keeps("a/B.CSV", FileFormat::Csv));
        // A stray parquet in a CSV folder is not this source's business.
        assert!(!keeps("a/b.parquet", FileFormat::Csv));
        assert!(!keeps("a/README", FileFormat::Csv));
        assert!(keeps("a/b.parquet", FileFormat::Parquet));
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;

    #[test]
    fn a_delta_table_is_found_by_its_log_not_its_extension() {
        // The log is what says "this directory is a table".
        assert!(keeps(
            "Tables/sales/_delta_log/00000000000000000000.json",
            FileFormat::Delta
        ));
        // The parquet inside belong to the log, not to the listing.
        assert!(!keeps("Tables/sales/part-0000.parquet", FileFormat::Delta));
        // And a Delta source is not a parquet source.
        assert!(!keeps("Tables/sales/_delta_log/0.json", FileFormat::Parquet));
    }
}
