//! Azure storage as a source: a blob container, an ADLS Gen2 filesystem, or a
//! Fabric OneLake workspace.
//!
//! **It syncs, it does not query remotely.** Alkyon's DuckDB runs with
//! `enable_external_access = false` and may load no extension, which is what
//! keeps a folder source from reading the rest of the machine — and it also means
//! DuckDB cannot open an `abfss://` URL. So the files come down to a local cache
//! first and are then read exactly as a folder source reads a folder: same
//! tables, same unions, same `source_file` column, same format options.
//!
//! The honest consequence is that there is **no predicate pushdown**. A parquet
//! file is fetched whole the first time it is seen, not scanned where it lives.
//! What makes that bearable is the cache: files are kept between runs and
//! re-fetched only when the service says their ETag changed, so the cost is paid
//! once per version of a file rather than once per query.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::files::FilesConnection;
use super::{Connection, Connector};
use crate::azure::storage::{self, Client, Entry};
use crate::error::{Error, Result};
use crate::model::{AuthConfig, Dialect, FileFormat, SourceConfig};

/// As many files as a folder source will look at. Past this a source has stopped
/// being something to browse.
const MAX_FILES: usize = 200;

/// How much a single source will pull down before it refuses. A guard against
/// pointing alkyon at a lake and waiting all afternoon without being told why.
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How long a sync is taken to still be current.
///
/// A connection is opened per query, and re-listing the filesystem before each
/// one would put an internet round trip in front of every statement. Half a
/// minute is short enough that a file dropped in a folder shows up while you are
/// still looking for it.
const RESYNC_AFTER: Duration = Duration::from_secs(30);

pub struct AdlsConnector {
    /// Where mirrored files live, one directory per source.
    cache: PathBuf,
    /// When each source last listed the remote, so that a burst of queries
    /// shares one listing.
    synced: Mutex<HashMap<PathBuf, Instant>>,
}

impl AdlsConnector {
    pub fn new(cache: PathBuf) -> Self {
        AdlsConnector {
            cache,
            synced: Mutex::new(HashMap::new()),
        }
    }

    /// The directory a source's mirror lives in.
    ///
    /// Named after where the files came from rather than after the source, so
    /// two sources pointing at the same folder share one copy, and renaming a
    /// source does not download everything again.
    fn mirror(&self, location: &storage::Location) -> PathBuf {
        let mut path = self.cache.join(sanitise(&location.host));
        path.push(sanitise(&location.filesystem));
        for segment in location.prefix.split('/').filter(|s| !s.is_empty()) {
            path.push(sanitise(segment));
        }
        path
    }

    fn due(&self, mirror: &Path) -> bool {
        match self.synced.lock().unwrap().get(mirror) {
            Some(last) => last.elapsed() > RESYNC_AFTER,
            None => true,
        }
    }

    fn mark_synced(&self, mirror: &Path) {
        self.synced
            .lock()
            .unwrap()
            .insert(mirror.to_path_buf(), Instant::now());
    }
}

/// A path segment safe to put on the local filesystem.
///
/// Remote names are not ours to trust: `..` in one would otherwise walk the
/// cache out of its own directory.
fn sanitise(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `.` and `..` are directories that already mean something.
    if cleaned.chars().all(|c| c == '.') {
        return "_".to_owned();
    }
    cleaned
}

/// Where a remote file is mirrored, keeping the shape of the tree.
///
/// The layout is what the tables are named after — a subdirectory is one table —
/// so the mirror has to keep the same shape, not flatten it.
fn local_path(mirror: &Path, relative: &str) -> PathBuf {
    let mut path = mirror.to_path_buf();
    for segment in relative.split('/').filter(|s| !s.is_empty()) {
        path.push(sanitise(segment));
    }
    path
}

/// The part of a listed path below the source's prefix.
///
/// The service answers paths relative to the *filesystem*, prefix included; the
/// mirror is rooted at the prefix, so that a source pointed at `sales/exports`
/// gives tables named after what is in `exports` and not after `exports` itself.
fn below(prefix: &str, path: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(path.to_owned());
    }
    path.strip_prefix(prefix)
        .map(|rest| rest.trim_start_matches('/').to_owned())
        .filter(|rest| !rest.is_empty())
}

/// Whether this source reads that file, by the format it declared.
fn keeps(path: &str, format: FileFormat) -> bool {
    let extension = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    extension.is_some_and(|e| format.extensions().contains(&e.as_str()))
}

/// The ETag of a mirrored file, as recorded when it was fetched.
///
/// Beside the file rather than in one index, so a half-finished sync leaves the
/// files it did fetch usable rather than invalidating the lot.
fn stamp_of(local: &Path) -> PathBuf {
    local.with_extension(format!(
        "{}.etag",
        local
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

fn cached_etag(local: &Path) -> Option<String> {
    if !local.is_file() {
        return None;
    }
    std::fs::read_to_string(stamp_of(local)).ok()
}

/// Bring the mirror in line with the remote, fetching only what changed.
async fn sync(client: &Client, mirror: &Path, format: FileFormat) -> Result<()> {
    let prefix = client.location().prefix.clone();
    let listed = client.list().await?;

    let wanted: Vec<(Entry, String)> = listed
        .into_iter()
        .filter_map(|entry| {
            let relative = below(&prefix, &entry.path)?;
            keeps(&relative, format).then_some((entry, relative))
        })
        .collect();

    if wanted.is_empty() {
        return Err(Error::BadRequest(format!(
            "no {} file under `{}/{}`",
            format.extensions().join(" or "),
            client.location().filesystem,
            prefix
        )));
    }
    if wanted.len() > MAX_FILES {
        return Err(Error::BadRequest(format!(
            "{} files under that path, and a source reads at most {MAX_FILES}. Point it \
             at a folder further in.",
            wanted.len()
        )));
    }

    let mut fetched = 0usize;
    let mut bytes = 0u64;
    for (entry, relative) in &wanted {
        let local = local_path(mirror, relative);
        // The ETag changes whenever the content does, so this is the whole of
        // the freshness question — no dates, no sizes, no guessing.
        if cached_etag(&local).as_deref() == Some(entry.etag.as_str()) && !entry.etag.is_empty() {
            continue;
        }
        bytes += entry.bytes;
        if bytes > MAX_BYTES {
            return Err(Error::BadRequest(format!(
                "that path holds more than {} GiB of new data, which alkyon will not \
                 mirror in one go. Point the source further in, or exclude what it \
                 does not need.",
                MAX_BYTES / (1024 * 1024 * 1024)
            )));
        }
        client.download(&entry.path, &local).await?;
        std::fs::write(stamp_of(&local), &entry.etag)?;
        fetched += 1;
    }

    // Files that are no longer there must stop being tables, or a source would
    // keep answering with a table nobody can find on the other end.
    let keep: std::collections::HashSet<PathBuf> = wanted
        .iter()
        .map(|(_, relative)| local_path(mirror, relative))
        .collect();
    prune(mirror, &keep);

    tracing::info!(
        files = wanted.len(),
        fetched,
        bytes,
        mirror = %mirror.display(),
        "synced an Azure storage source"
    );
    Ok(())
}

/// Delete everything in the mirror that the listing no longer names.
fn prune(mirror: &Path, keep: &std::collections::HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(mirror) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune(&path, keep);
            // Empty afterwards means it held only files that are gone.
            let _ = std::fs::remove_dir(&path);
        } else if !keep.contains(&path) && path.extension().is_some_and(|e| e != "etag") {
            let _ = std::fs::remove_file(stamp_of(&path));
            let _ = std::fs::remove_file(&path);
        }
    }
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
                 type it holds (csv, parquet, json, json_lines or excel).",
                config.id
            ))
        })?;

        let mirror = self.mirror(&location);
        if self.due(&mirror) {
            let AuthConfig::AadToken { token } = &config.auth else {
                return Err(Error::Unsupported(
                    "an Azure storage source is reached with a Microsoft Entra sign-in".into(),
                ));
            };
            let client = Client::new(location, token.clone())?;
            std::fs::create_dir_all(&mirror)?;
            sync(&client, &mirror, format).await?;
            self.mark_synced(&mirror);
        }

        // From here it is a folder of files like any other, which is the point:
        // the tree, the unions, the spreadsheets and the federation all work
        // without knowing where the bytes came from.
        Ok(Box::new(FilesConnection::over(mirror, config.options.clone())))
    }

    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_name_cannot_walk_out_of_the_cache() {
        let mirror = PathBuf::from("/cache/contoso/sales");
        let escaped = local_path(&mirror, "../../../etc/passwd");
        assert!(
            escaped.starts_with(&mirror),
            "{} escaped the mirror",
            escaped.display()
        );
        assert_eq!(sanitise(".."), "_");
        assert_eq!(sanitise("."), "_");
        assert_eq!(sanitise("2026-01_final.csv"), "2026-01_final.csv");
        assert_eq!(sanitise("a b/c"), "a_b_c");
    }

    #[test]
    fn the_mirror_keeps_the_shape_of_the_tree() {
        let mirror = PathBuf::from("/cache/x");
        assert_eq!(
            local_path(&mirror, "exports/2026/january.csv"),
            mirror.join("exports").join("2026").join("january.csv")
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
        // No prefix: everything is below it.
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
        assert!(keeps("a/b.xlsx", FileFormat::Excel));
    }

    #[test]
    fn the_etag_sits_beside_the_file_it_describes() {
        let stamp = stamp_of(Path::new("/cache/x/a.csv"));
        assert_eq!(stamp, PathBuf::from("/cache/x/a.csv.etag"));
        // And never collides with the file itself, whatever it is called.
        assert_ne!(stamp, PathBuf::from("/cache/x/a.csv"));
    }
}
