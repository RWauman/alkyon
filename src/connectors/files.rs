//! A folder — or a single file — as a source.
//!
//! It is a [`Connector`] like any other rather than a second kind of registry
//! entry, and that pays for itself twice over: the sources pane, the scopes, the
//! green dot, the explorer tree, autocompletion and the schema search all work
//! unchanged, and `-- @import x = my-folder : select …` federates a folder
//! against a database with no new machinery.
//!
//! **A file at the root is a table**, named after itself, and **a subdirectory is
//! one table** over every file inside it — unioned, with a [`FILE_COLUMN`] saying
//! which file each row came from, so a folder of monthly exports is one table
//! rather than twelve. The files of one subdirectory must agree on their columns;
//! one that does not is an error rather than a quiet reshaping.
//!
//! Everything lands in the schema [`ROOT_SCHEMA`], under a catalogue named after the
//! folder: `parquet → public → customers`. The root directory therefore has no name
//! left to lend a table, which is why its files keep their own.
//!
//! Which format is not guessed: a folder source declares it, and that is also what
//! narrows a mixed directory to the files worth reading.
//!
//! The session is the same confined DuckDB the federator uses: external access
//! off, this path as the only exception, no extension may be installed, and the
//! configuration is frozen before any user SQL runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;

use super::{Connection, Connector};
use crate::error::{Error, Result};
use crate::federation::{
    self, quote_identifier, quote_literal, Materialised, Sandbox,
};
use crate::model::{
    ColumnInfo, ColumnMeta, CsvOptions, Dialect, FileFormat, FileOptions, LogicalType, RowBatch,
    SourceConfig, SourceKind, TableInfo, TableKind, TableSchema,
};

/// A folder source is browsed, not crawled. Past this the tree stops being
/// useful and every snapshot would read hundreds of file headers.
const MAX_FILES: usize = 200;
const MAX_DEPTH: usize = 6;

/// The schema every table lands in — `public`, as psql, rather than DuckDB's own
/// `main`. It is created and made the default search path when the session opens.
const ROOT_SCHEMA: &str = "public";

/// The column a folder source adds, holding the file a row came from without its
/// extension. A single-file source has nothing to tell apart, so it has no such
/// column.
const FILE_COLUMN: &str = "source_file";

/// The DuckDB table function that reads a format, or `None` for Excel — which
/// calamine reads in this process because DuckDB's `excel` extension would have to
/// be fetched.
fn reader(format: FileFormat) -> Option<&'static str> {
    match format {
        FileFormat::Csv => Some("read_csv"),
        FileFormat::Parquet => Some("read_parquet"),
        // One function for both: `read_json` auto-detects whether the file is an
        // array of documents or one per line. The formats stay separate anyway,
        // because declaring which you have is how a `.txt` full of JSON gets read.
        FileFormat::Json | FileFormat::JsonLines => Some("read_json"),
        // A directory, not a list of files, and the `delta` extension works out
        // which parquet inside it are live by reading `_delta_log`.
        FileFormat::Delta => Some("delta_scan"),
        FileFormat::Excel => None,
    }
}

/// The name `_delta_log` — the only thing that makes a directory a Delta table.
const DELTA_LOG: &str = "_delta_log";

pub(crate) fn reader_for(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_string_lossy().into_owned();
    reader(FileFormat::from_extension(&extension)?)
}

pub(crate) fn readable_extensions() -> String {
    let mut seen: Vec<&str> = Vec::new();
    for format in FileFormat::ALL {
        for extension in format.extensions() {
            if !seen.contains(extension) {
                seen.push(extension);
            }
        }
    }
    seen.join(", ")
}

/// A CSV option, rendered as the named argument DuckDB knows it by.
fn csv_arguments(options: &CsvOptions) -> Vec<String> {
    let mut out = Vec::new();
    let text = |name: &str, value: &Option<String>| {
        value
            .as_ref()
            .filter(|value| !value.is_empty())
            .map(|value| format!("{name} = {}", quote_literal(value)))
    };

    out.extend(text("delim", &options.delimiter));
    out.extend(text("quote", &options.quote));
    out.extend(text("escape", &options.escape));
    out.extend(text("decimal_separator", &options.decimal));
    out.extend(text("encoding", &options.encoding));
    out.extend(text("nullstr", &options.null_string));
    out.extend(text("dateformat", &options.date_format));
    out.extend(text("timestampformat", &options.timestamp_format));
    if let Some(header) = options.header {
        out.push(format!("header = {header}"));
    }
    if let Some(skip) = options.skip {
        out.push(format!("skip = {skip}"));
    }
    if let Some(ignore) = options.ignore_errors {
        out.push(format!("ignore_errors = {ignore}"));
    }
    if let Some(size) = options.sample_size {
        out.push(format!("sample_size = {size}"));
    }
    if let Some(all) = options.all_varchar {
        out.push(format!("all_varchar = {all}"));
    }
    out
}

/// `read_json` needs telling which shape it is looking at when the extension does
/// not say — `format = 'newline_delimited'` against an array of documents is the
/// difference between one table and an error.
fn json_arguments(format: FileFormat) -> Vec<String> {
    match format {
        FileFormat::JsonLines => vec!["format = 'newline_delimited'".to_owned()],
        _ => Vec::new(),
    }
}

/// One directory's files, as one table — or the single file a file source names.
#[derive(Debug, Clone)]
struct DataSet {
    /// The view name: the directory's own name, the source folder's for the root,
    /// or the file's stem for a file source.
    name: String,
    /// Every file it unions, alphabetical. Exactly one for a file source.
    ///
    /// An **address**, not a path: a local file with forward slashes, or an
    /// `abfss://` URL when the source is a storage account. Everything below
    /// hands it to DuckDB as a literal, which is what lets one piece of code
    /// serve a folder on this machine and a container in Azure.
    files: Vec<String>,
    format: FileFormat,
    /// Which sheet, for spreadsheets. `None` for every other format, and for a
    /// workbook whose sheets could not be listed.
    sheet: Option<String>,
    /// Whether the rows carry [`FILE_COLUMN`]. A folder tags them; a file source
    /// has nothing to tell apart.
    tagged: bool,
}

impl DataSet {
    /// The `SELECT` whose rows are this table.
    ///
    /// A list of paths rather than a glob: the files have already been chosen — by
    /// the declared format, and by the source's file option — and a glob would
    /// also have to escape the `*` and `[` a perfectly ordinary filename may hold.
    ///
    /// `None` for a spreadsheet: there is no DuckDB function to call, so it gets
    /// materialised instead.
    fn query(&self, options: &FileOptions) -> Option<String> {
        let function = reader(self.format)?;

        // A Delta table is one directory handed to `delta_scan`, and the log
        // inside it says which files are live. There is nothing to union and no
        // `filename` argument to ask for.
        if self.format.is_delta() {
            let directory = self.files.first()?;
            return Some(format!(
                "SELECT * FROM {function}({})",
                quote_literal(directory)
            ));
        }
        // Forward-slashed already, when the address was made: that is what DuckDB
        // normalises paths to when it checks them against the sandbox, so the
        // literal and the permission are written the same way.
        let paths: Vec<String> = self.files.iter().map(|file| quote_literal(file)).collect();

        let mut arguments = vec![if self.tagged {
            format!("[{}]", paths.join(", "))
        } else {
            paths.concat()
        }];
        match self.format {
            FileFormat::Csv => arguments.extend(csv_arguments(&options.csv)),
            FileFormat::Json | FileFormat::JsonLines => {
                arguments.extend(json_arguments(self.format))
            }
            _ => {}
        }
        if self.tagged {
            arguments.push("filename = true".to_owned());
        }
        let scan = format!("{function}({})", arguments.join(", "));

        // `filename` is the whole path, which is not what anybody wants to group
        // by, so it is traded for the stem. `separator := 'both'` because the
        // literals are written with forward slashes on every platform.
        Some(if self.tagged {
            format!(
                "SELECT * EXCLUDE (filename), \
                 parse_filename(filename, trim_extension := true, separator := 'both') AS {} \
                 FROM {scan}",
                quote_identifier(FILE_COLUMN)
            )
        } else {
            format!("SELECT * FROM {scan}")
        })
    }

    /// What to name in an error: the one file, or the directory holding them.
    fn label(&self) -> String {
        match self.files.as_slice() {
            [only] => only.clone(),
            files => files
                .first()
                .and_then(|file| file.rsplit_once('/'))
                .map(|(directory, _)| format!("{directory} ({} files)", files.len()))
                .unwrap_or_else(|| self.name.clone()),
        }
    }
}

/// One directory's worth of readable files, before it is named.
struct Directory {
    /// Relative to the source root, with forward slashes. Empty at the root.
    relative: String,
    /// Addresses, in the sense [`DataSet::files`] means.
    files: Vec<String>,
}

/// What `root` holds, as tables.
///
/// **A file at the root is its own table**, named after itself: the root directory
/// has no name left to give — the source's catalogue already carries it — and
/// `parquet.public.parquet` says nothing twice. **A subdirectory is one table** over
/// every file in it, which is where a folder of monthly exports belongs.
fn walk(root: &Path, format: FileFormat, options: &FileOptions) -> Vec<DataSet> {
    if format.is_delta() {
        return delta_tables(root, options);
    }
    let mut directories = Vec::new();
    let mut count = 0;
    collect(
        root,
        root,
        0,
        Some(format),
        &options.exclude,
        &mut directories,
        &mut count,
    );
    name_sets(directories, format, options)
}

/// The Delta tables a remote listing implies.
///
/// `_delta_log/…` inside a directory is what says the directory is a table, so
/// the tables are the parents of those logs — deduplicated, because a log holds
/// many files and each one names the same table.
fn delta_from_listing(listing: &[(String, String)], options: &FileOptions) -> Vec<DataSet> {
    let mut taken: HashSet<String> = HashSet::new();
    let mut found: Vec<(String, String)> = Vec::new();

    for (relative, address) in listing {
        // Everything before `/_delta_log/`: the table, relative to the prefix.
        let marker = format!("{DELTA_LOG}/");
        let Some(cut) = relative.find(&marker) else {
            continue;
        };
        let table = relative[..cut].trim_end_matches('/').to_owned();
        if options
            .exclude
            .iter()
            .any(|left_out| left_out.eq_ignore_ascii_case(&table))
        {
            continue;
        }
        // The same cut on the address, so the URL names the directory and not the
        // log file inside it.
        let Some(at) = address.find(&marker) else {
            continue;
        };
        let directory = address[..at].trim_end_matches('/').to_owned();

        if !found.iter().any(|(seen, _)| *seen == table) {
            found.push((table, directory));
        }
    }

    let mut sets: Vec<DataSet> = found
        .into_iter()
        .map(|(relative, address)| DataSet {
            name: if relative.is_empty() {
                stem_for(&address, &mut taken)
            } else {
                name_for(&relative, &mut taken)
            },
            files: vec![address],
            format: FileFormat::Delta,
            sheet: None,
            tagged: false,
        })
        .collect();
    sets.sort_by(|a, b| a.name.cmp(&b.name));
    sets
}

/// Every Delta table under `root`: each directory holding a `_delta_log`.
///
/// A different shape of walk from the one below, because a Delta table is a
/// directory rather than a group of files, and the directory holding it is not to
/// be descended into — the parquet inside are the table's business, not ours.
fn delta_tables(root: &Path, options: &FileOptions) -> Vec<DataSet> {
    let mut found = Vec::new();
    find_delta(root, root, 0, &options.exclude, &mut found);

    let mut taken: HashSet<String> = HashSet::new();
    let mut sets: Vec<DataSet> = found
        .into_iter()
        .map(|(relative, address)| DataSet {
            // The root itself is a table when it is one, and takes the source's
            // own name the way a root file does.
            name: if relative.is_empty() {
                stem_for(&address, &mut taken)
            } else {
                name_for(&relative, &mut taken)
            },
            files: vec![address],
            format: FileFormat::Delta,
            sheet: None,
            tagged: false,
        })
        .collect();
    sets.sort_by(|a, b| a.name.cmp(&b.name));
    sets
}

fn find_delta(
    root: &Path,
    directory: &Path,
    depth: usize,
    excluded: &[String],
    out: &mut Vec<(String, String)>,
) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(children) = std::fs::read_dir(directory) else {
        return;
    };

    let mut subdirectories = Vec::new();
    let mut is_table = false;
    for child in children.flatten() {
        let name = child.file_name().to_string_lossy().into_owned();
        if !child.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if name == DELTA_LOG {
            is_table = true;
        } else if !name.starts_with('.') && !crate::workspace::SKIP.contains(&name.as_str()) {
            subdirectories.push(child.path());
        }
    }

    let relative = relative_of(root, directory);
    if is_table {
        // A table is a leaf: what is inside belongs to the log, and a Delta table
        // nested in another is not a thing.
        if !excluded.iter().any(|left_out| left_out.eq_ignore_ascii_case(&relative)) {
            out.push((relative, address_of(directory)));
        }
        return;
    }

    subdirectories.sort();
    for subdirectory in subdirectories {
        find_delta(root, &subdirectory, depth + 1, excluded, out);
    }
}

/// Turn grouped directories into named tables.
///
/// Split from [`walk`] because the grouping is the same wherever the files are:
/// a storage account is listed over HTTP rather than walked, and then wants
/// exactly these rules — the root's files under their own names, a subdirectory
/// under its own, and a spreadsheet expanded per sheet.
fn name_sets(
    mut directories: Vec<Directory>,
    format: FileFormat,
    options: &FileOptions,
) -> Vec<DataSet> {
    let mut taken: HashSet<String> = HashSet::new();
    let mut sets = Vec::new();

    // The root first, so its file names win a collision with a subdirectory's.
    if let Some(at) = directories.iter().position(|d| d.relative.is_empty()) {
        for file in directories.remove(at).files {
            sets.push(DataSet {
                name: stem_for(&file, &mut taken),
                files: vec![file],
                format,
                sheet: None,
                tagged: false,
            });
        }
    }
    for directory in directories {
        sets.push(DataSet {
            name: name_for(&directory.relative, &mut taken),
            files: directory.files,
            format,
            sheet: None,
            tagged: true,
        });
    }

    let mut sets = expand_sheets(sets, options);
    sets.sort_by(|a, b| a.name.cmp(&b.name));
    sets
}

/// The files a folder source could read, relative to its root and with forward
/// slashes — what the source dialog's file list offers.
///
/// `options.exclude` is deliberately ignored: the list has to keep offering the
/// files that were excluded, or they could never be put back.
pub fn candidates(root: &Path, options: &FileOptions) -> Vec<String> {
    let mut directories = Vec::new();
    let mut count = 0;
    collect(
        root,
        root,
        0,
        options.format,
        &[],
        &mut directories,
        &mut count,
    );

    let mut out: Vec<String> = directories
        .into_iter()
        .flat_map(|directory| {
            directory.files.into_iter().map(move |file| {
                let name = filename_of(&file).to_owned();
                if directory.relative.is_empty() {
                    name
                } else {
                    format!("{}/{name}", directory.relative)
                }
            })
        })
        .collect();
    out.sort();
    out
}

fn collect(
    root: &Path,
    directory: &Path,
    depth: usize,
    format: Option<FileFormat>,
    excluded: &[String],
    out: &mut Vec<Directory>,
    count: &mut usize,
) {
    if depth > MAX_DEPTH || *count >= MAX_FILES {
        return;
    }
    // An unreadable subdirectory should not fail the whole listing.
    let Ok(children) = std::fs::read_dir(directory) else {
        return;
    };

    let mut files = Vec::new();
    let mut subdirectories = Vec::new();
    for child in children.flatten() {
        let path = child.path();
        let name = child.file_name().to_string_lossy().into_owned();
        let Ok(kind) = child.file_type() else {
            continue;
        };

        if kind.is_dir() {
            if !name.starts_with('.') && !crate::workspace::SKIP.contains(&name.as_str()) {
                subdirectories.push(path);
            }
        } else if kind.is_file() && *count < MAX_FILES && keeps(root, &path, format, excluded) {
            *count += 1;
            // Forward-slashed here, once, so that everything downstream can treat
            // a local file and a remote URL as the same kind of string.
            files.push(address_of(&path));
        }
    }

    // Alphabetical, so the union's row order and the file list are stable rather
    // than whatever the filesystem hands back.
    files.sort();
    subdirectories.sort();
    if !files.is_empty() {
        out.push(Directory {
            relative: relative_of(root, directory),
            files,
        });
    }
    for subdirectory in subdirectories {
        collect(root, &subdirectory, depth + 1, format, excluded, out, count);
    }
}

/// Whether this source reads `path`.
///
/// A **declared** format narrows the folder to that format's own extensions: say
/// "parquet" and a stray `notes.csv` stops being data. `excluded` then drops the
/// files the source was told to leave out.
fn keeps(root: &Path, path: &Path, format: Option<FileFormat>, excluded: &[String]) -> bool {
    let extension = path.extension().map(|e| e.to_string_lossy().into_owned());
    let readable = match format {
        Some(declared) => extension.as_deref().is_some_and(|e| {
            declared
                .extensions()
                .contains(&e.to_ascii_lowercase().as_str())
        }),
        None => extension
            .as_deref()
            .and_then(FileFormat::from_extension)
            .is_some(),
    };
    if !readable {
        return false;
    }
    if excluded.is_empty() {
        return true;
    }
    // Case-insensitively: the value came back from a listing of this very folder,
    // and on Windows it may not have kept the case it was written in.
    let relative = relative_of(root, path);
    !excluded
        .iter()
        .any(|left_out| left_out.eq_ignore_ascii_case(&relative))
}

/// `path` relative to `root`, with forward slashes. Empty when they are the same.
fn relative_of(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => String::new(),
    }
}

/// The table name for a file at the root: its stem, `customers.parquet` being
/// `customers`.
///
/// A stem already claimed keeps the whole filename instead of vanishing — which is
/// how `notes.csv` and `notes.txt`, both CSV as far as the source is concerned, can
/// both be there.
fn stem_for(address: &str, taken: &mut HashSet<String>) -> String {
    let filename = filename_of(address);
    let stem = match filename.rsplit_once('.') {
        // A leading dot is a name, not an extension: `.gitignore` has no stem.
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => filename,
    };
    let stem = if stem.is_empty() { "data" } else { stem };

    if taken.insert(stem.to_owned()) {
        return stem.to_owned();
    }
    unique(filename.to_owned(), taken)
}

/// The last segment of an address. Addresses are forward-slashed wherever they
/// came from, which is what makes this one line rather than two platforms.
fn filename_of(address: &str) -> &str {
    address.rsplit('/').next().unwrap_or(address)
}

/// A local path as an address: forward slashes, because that is what DuckDB
/// normalises to and what a remote URL uses anyway.
fn address_of(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The table name for one subdirectory: its own name.
fn name_for(relative: &str, taken: &mut HashSet<String>) -> String {
    let own = relative
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(relative)
        .to_owned();
    if taken.insert(own.clone()) {
        return own;
    }
    // Two directories of the same name under different parents, or one named
    // after a file at the root: the loser keeps its whole path rather than
    // vanishing.
    unique(relative.replace('/', "_"), taken)
}

/// `name`, or the first of `name_2`, `name_3`… that nothing else has claimed.
///
/// The last resort, and it has to exist: `sales.csv` beside a `sales/` directory is
/// two perfectly good tables with one good name between them, and dropping either
/// would make the explorer lie about the folder.
fn unique(name: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.clone()) {
        return name;
    }
    for suffix in 2.. {
        let candidate = format!("{name}_{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("the loop only ends by returning")
}

/// Replace each spreadsheet data set with one per sheet.
///
/// Done at discovery so that everything downstream — the tree, the snapshot, the
/// name matching in [`FilesConnection::referenced`] — agrees on what the tables
/// are. Without it the explorer offered `book` while the query needed
/// `book_Customers`, which is the explorer lying about the source.
///
/// The first file says what the sheets are, since the set's files are required to
/// agree anyway. A workbook that cannot be opened keeps its single entry: the error
/// then arrives when the table is used, with calamine's own message, instead of the
/// file vanishing from the tree.
fn expand_sheets(sets: Vec<DataSet>, options: &FileOptions) -> Vec<DataSet> {
    let mut out = Vec::with_capacity(sets.len());
    for set in sets {
        if !set.format.is_excel() {
            out.push(set);
            continue;
        }

        // A named sheet is the only one that matters, and it keeps the set's name.
        if let Some(wanted) = options.excel.sheet.clone() {
            out.push(DataSet {
                sheet: Some(wanted),
                ..set
            });
            continue;
        }

        let sheets = set
            .files
            .first()
            .map(|file| crate::federation::excel::sheet_names(Path::new(file)));
        match sheets {
            Some(Ok(sheets)) if sheets.len() == 1 => out.push(DataSet {
                sheet: Some(sheets[0].clone()),
                ..set
            }),
            Some(Ok(sheets)) if !sheets.is_empty() => {
                for sheet in sheets {
                    out.push(DataSet {
                        name: format!("{}_{}", set.name, sheet),
                        sheet: Some(sheet),
                        files: set.files.clone(),
                        format: set.format,
                        tagged: set.tagged,
                    });
                }
            }
            _ => out.push(set),
        }
    }
    out
}

pub struct FilesConnector;

#[async_trait]
impl Connector for FilesConnector {
    async fn connect(&self, config: &SourceConfig) -> Result<Box<dyn Connection>> {
        let path = config
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "source `{}` is a folder or file source and needs a path",
                    config.id
                ))
            })?;

        // Resolved now rather than when the source was saved, so a project
        // registry stays portable and a disconnected drive shows a red dot
        // instead of having been baked in wrong.
        let root = crate::workspace::resolve_root(path)?;
        let single = config.kind == SourceKind::File;

        // The kind is a promise about the path, so check it rather than quietly
        // doing whichever the filesystem happens to allow.
        if single && !root.is_file() {
            return Err(Error::BadRequest(format!(
                "`{path}` is a folder — register it as a folder source, or point this \
                 one at a file inside it"
            )));
        }
        if !single && !root.is_dir() {
            return Err(Error::BadRequest(format!(
                "`{path}` is a file — register it as a file source, or point this one \
                 at the folder holding it"
            )));
        }

        let options = config.options.clone();
        if single {
            // A file source names its file outright, so a declared format wins over
            // the extension: that is how `export.dat` gets read as CSV.
            let extension = root.extension().map(|e| e.to_string_lossy().into_owned());
            if options.format_for(extension.as_deref()).is_none() {
                return Err(Error::BadRequest(format!(
                    "`{path}` is not a format alkyon reads ({}). Say which format it is \
                     in the source's options if the extension does not give it away.",
                    readable_extensions()
                )));
            }
        } else if options.format.is_none() {
            // A folder's files are read as one table per directory, and files of
            // two formats cannot be unioned — so which format it holds is the one
            // thing a folder source has to be told.
            return Err(Error::BadRequest(format!(
                "source `{}` is a folder source and must say which file type it holds \
                 (csv, parquet, json, json_lines or excel).",
                config.id
            )));
        }

        Ok(Box::new(FilesConnection {
            where_: Place::Local { root, single },
            options,
        }))
    }

    fn dialect(&self) -> Dialect {
        Dialect::DuckDb
    }
}

pub struct FilesConnection {
    where_: Place,
    options: FileOptions,
}

/// Where a source's files are, and therefore what sort of session reads them.
///
/// The only thing that differs between the two: what the tables are worked out
/// from, and how the DuckDB session is confined. Everything after that — the
/// tree, the columns, the unions, the `source_file` column, `@import` — is the
/// same code, because by then a file is just an address in a literal.
enum Place {
    /// A folder, or one file, on the machine running alkyon.
    Local { root: PathBuf, single: bool },
    /// A container in Azure storage: listed over HTTPS when the connection is
    /// opened, then read where it lies by DuckDB's `azure` extension. Nothing is
    /// copied here.
    Azure {
        access: crate::federation::AzureAccess,
        /// Worked out at connect time from the listing, because a listing is a
        /// network round trip and the tree asks for the tables repeatedly.
        sets: Vec<DataSet>,
    },
}

impl FilesConnection {
    /// Read `root` as a folder of data files, with no path from a person to
    /// check first.
    ///
    /// Only the tests use it today — [`FilesConnector::connect`] is the way in
    /// for a source someone registered. Kept because it is the honest
    /// constructor for "this directory, as tables", and because it is what the
    /// grouping is tested through.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn over(root: PathBuf, options: FileOptions) -> Self {
        FilesConnection {
            where_: Place::Local {
                root,
                single: false,
            },
            options,
        }
    }

    /// A container in Azure storage, from a listing of it.
    ///
    /// `listing` pairs each file's path **below the source's prefix** — which is
    /// what the tables are named from — with the address DuckDB will read. The
    /// grouping is then the same one a folder gets, which is the point: a
    /// directory in a container is one table over its files, exactly as on disk.
    pub(crate) fn in_azure(
        access: crate::federation::AzureAccess,
        listing: &[(String, String)],
        format: FileFormat,
        options: FileOptions,
    ) -> Self {
        // A Delta table is a directory, and the listing names files inside it —
        // so the tables are the directories holding a `_delta_log`, taken once
        // each rather than one per file.
        if format.is_delta() {
            return FilesConnection {
                where_: Place::Azure {
                    access,
                    sets: delta_from_listing(listing, &options),
                },
                options,
            };
        }

        let mut directories: Vec<Directory> = Vec::new();
        for (relative, address) in listing {
            let folder = match relative.rsplit_once('/') {
                Some((folder, _)) => folder,
                None => "",
            };
            match directories.iter_mut().find(|d| d.relative == folder) {
                Some(directory) => directory.files.push(address.clone()),
                None => directories.push(Directory {
                    relative: folder.to_owned(),
                    files: vec![address.clone()],
                }),
            }
        }
        // Alphabetical, so a union's row order and the tree are stable rather than
        // whatever order the service listed them in.
        for directory in &mut directories {
            directory.files.sort();
        }
        directories.sort_by(|a, b| a.relative.cmp(&b.relative));

        FilesConnection {
            where_: Place::Azure {
                access,
                sets: name_sets(directories, format, &options),
            },
            options,
        }
    }

    /// How to open a session for this source, as something owned.
    ///
    /// Owned because every read happens on a blocking worker — DuckDB is
    /// synchronous — and a worker cannot borrow the connection it was spawned
    /// from.
    fn recipe(&self) -> Recipe {
        // Only what this source cannot be read without: `delta` is fetched once,
        // and every other format is already linked in.
        let needs = self
            .options
            .format
            .and_then(FileFormat::extension)
            .map(str::to_owned);
        match &self.where_ {
            Place::Local { .. } => Recipe::Confined(self.sandbox(), needs),
            Place::Azure { access, .. } => Recipe::Azure(access.clone(), needs),
        }
    }

    /// A single file is granted as a file, not as its directory — pointing a
    /// source at one spreadsheet should not hand over everything beside it.
    fn sandbox(&self) -> Sandbox {
        match &self.where_ {
            Place::Local { root, single: true } => Sandbox::file(root.clone()),
            Place::Local { root, .. } => Sandbox::directory(root.clone()),
            // Nothing on this machine is granted, because nothing on it is read.
            Place::Azure { .. } => Sandbox::default(),
        }
    }

    fn sets(&self) -> Vec<DataSet> {
        let (root, single) = match &self.where_ {
            // Listed when the connection opened; a network round trip is not a
            // thing to repeat for every question about the tree.
            Place::Azure { sets, .. } => return sets.clone(),
            Place::Local { root, single } => (root, *single),
        };

        if single {
            let extension = root.extension().map(|e| e.to_string_lossy().into_owned());
            let Some(format) = self.options.format_for(extension.as_deref()) else {
                return Vec::new();
            };
            let name = root
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "data".to_owned());
            return expand_sheets(
                vec![DataSet {
                    name,
                    files: vec![address_of(root)],
                    format,
                    sheet: None,
                    tagged: false,
                }],
                &self.options,
            );
        }
        // Guaranteed by `connect`; an empty catalogue is the honest answer if a
        // record from disk somehow lacks it.
        let Some(format) = self.options.format else {
            return Vec::new();
        };
        walk(root, format, &self.options)
    }

    /// The tables whose name appears anywhere in `sql`.
    ///
    /// Creating a view makes DuckDB bind it, which reads the files' headers — so
    /// defining every view on every query would mean reading two hundred headers
    /// to answer a query against one. A view can only be referenced if its name
    /// occurs in the text, so this drops work without ever dropping a table the
    /// query could have used.
    fn referenced(&self, sql: &str) -> Vec<DataSet> {
        let haystack = sql.to_lowercase();
        self.sets()
            .into_iter()
            .filter(|set| haystack.contains(&set.name.to_lowercase()))
            .collect()
    }

    /// Add the missing sentence to a parser error, when this source has names
    /// that explain it.
    fn explain(&self, error: Error) -> Error {
        let Error::Federated(message) = &error else {
            return error;
        };
        if !message.contains("syntax error") && !message.contains("Parser Error") {
            return error;
        }
        let awkward = self.awkward_names();
        if awkward.is_empty() {
            return error;
        }
        let quoted: Vec<String> = awkward.iter().map(|name| format!("\"{name}\"")).collect();
        Error::Federated(format!(
            "{message}\n\nThis source has tables whose names SQL reads as something \
             other than a name, so they must be quoted: {}. Double-clicking in the \
             explorer writes them for you.",
            quoted.join(", ")
        ))
    }

    /// Names this source exposes that SQL will not accept unquoted.
    ///
    /// A directory called `2022` is a perfectly good directory and a table name
    /// that reads as a number, so `select * from 2022` is a syntax error and DuckDB
    /// says only "syntax error". Alkyon knows exactly which of its own names have
    /// that problem, so it can say.
    fn awkward_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for set in self.sets() {
            if !is_bare_identifier(&set.name) && !names.contains(&set.name) {
                names.push(set.name);
            }
        }
        names
    }
}

/// Whether SQL will read `name` as an identifier without quotes around it.
fn is_bare_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Define `sets` as views.
///
/// Creating a view makes DuckDB bind it, which reads the files' headers — so this
/// is where a malformed CSV, a truncated parquet or two files that do not agree on
/// their columns is found out. The error is *returned* rather than swallowed: these
/// are the tables the query named, and "the sniffer could not read line 4 of
/// customers.csv" is a far better answer than the `table customers does not exist`
/// that skipping it would produce.
///
/// Enumeration paths — the tree, the snapshot — want the opposite and keep
/// going, so they do not call this.
fn define(connection: &duckdb::Connection, sets: &[DataSet], options: &FileOptions) -> Result<()> {
    for set in sets {
        // A spreadsheet has no reader function, so it becomes a real table filled
        // from calamine rather than a view over the files. That is eager, and the
        // reason it is acceptable: `define` is only ever called for the tables a
        // query actually named.
        let Some(query) = set.query(options) else {
            materialise_excel(connection, set)?;
            continue;
        };

        let sql = format!(
            "CREATE VIEW {}.{} AS {query};",
            quote_identifier(ROOT_SCHEMA),
            quote_identifier(&set.name),
        );
        connection
            .execute_batch(&sql)
            .map_err(|e| Error::Federated(format!("{}: {e}", set.label())))?;
    }
    Ok(())
}

/// Read a data set's spreadsheets with calamine and hand DuckDB one typed table.
///
/// The files have to agree on their columns, and a set of one is the ordinary case
/// — so the check costs nothing and the message says which file broke it, which
/// beats DuckDB reporting a cast failure three columns later.
fn materialise_excel(connection: &duckdb::Connection, set: &DataSet) -> Result<()> {
    let mut columns: Option<Vec<ColumnMeta>> = None;
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();

    for file in &set.files {
        let read = crate::federation::excel::read(Path::new(file), set.sheet.as_deref())?;
        match &columns {
            None => columns = Some(read.columns.clone()),
            Some(first) => {
                if !same_columns(first, &read.columns) {
                    return Err(Error::Federated(format!(
                        "{}: its columns are {} where the other files in {} have {}. \
                         Every file in one folder must have the same columns.",
                        file,
                        names_of(&read.columns),
                        set.name,
                        names_of(first),
                    )));
                }
            }
        }

        let name = filename_of(file);
        let stem = match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
            _ => name.to_owned(),
        };
        for mut row in read.rows {
            if set.tagged {
                row.push(Some(stem.clone()));
            }
            rows.push(row);
        }
    }

    let mut columns = columns.unwrap_or_default();
    if set.tagged {
        columns.push(file_column());
    }
    federation::load(
        connection,
        &Materialised {
            alias: set.name.clone(),
            columns,
            rows,
        },
    )
}

/// What [`FILE_COLUMN`] looks like on the materialised path, where there is no
/// DuckDB expression to infer it from.
fn file_column() -> ColumnMeta {
    ColumnMeta {
        name: FILE_COLUMN.to_owned(),
        type_name: "varchar".to_owned(),
        logical: LogicalType::Text,
    }
}

fn same_columns(a: &[ColumnMeta], b: &[ColumnMeta]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.name == b.name)
}

fn names_of(columns: &[ColumnMeta]) -> String {
    columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The columns DuckDB infers for one data set.
fn describe(
    connection: &duckdb::Connection,
    set: &DataSet,
    options: &FileOptions,
) -> Result<Vec<ColumnInfo>> {
    // A spreadsheet has to be read to be described, and calamine already reports
    // the types it inferred, so there is nothing for DuckDB to add. The first file
    // is enough: the rest are required to match it.
    let Some(query) = set.query(options) else {
        let file = set
            .files
            .first()
            .ok_or_else(|| Error::BadRequest(format!("`{}` has no files", set.name)))?;
        let read = crate::federation::excel::read(Path::new(file), set.sheet.as_deref())?;
        let mut columns = read.columns;
        if set.tagged {
            columns.push(file_column());
        }
        return Ok(columns
            .iter()
            .enumerate()
            .map(|(index, column)| ColumnInfo {
                name: column.name.clone(),
                ordinal: index as i32 + 1,
                data_type: format!("{:?}", column.logical).to_lowercase(),
                nullable: true,
                is_primary_key: false,
                default: None,
            })
            .collect());
    };

    let sql = format!("DESCRIBE {query}");
    let mut statement = connection.prepare(&sql).map_err(federated)?;
    let mut rows = statement.query([]).map_err(federated)?;

    let mut columns = Vec::new();
    let mut ordinal = 1;
    while let Some(row) = rows.next().map_err(federated)? {
        columns.push(ColumnInfo {
            name: row.get::<_, String>(0).map_err(federated)?,
            ordinal,
            data_type: row.get::<_, String>(1).map_err(federated)?.to_lowercase(),
            // A file has no constraints to report: everything may be missing and
            // nothing is a key.
            nullable: true,
            is_primary_key: false,
            default: None,
        });
        ordinal += 1;
    }
    Ok(columns)
}

fn federated(e: duckdb::Error) -> Error {
    Error::Federated(e.to_string())
}

/// How to open a session, carried by value into a blocking worker.
enum Recipe {
    /// External access off, one path granted, plus any extension the format needs
    /// — loaded before the door is shut, which is the only order that works.
    Confined(Sandbox, Option<String>),
    /// External access left on for one storage account, the local filesystem
    /// shut. See [`crate::federation::open_duckdb_azure`].
    Azure(crate::federation::AzureAccess, Option<String>),
}

impl Recipe {
    fn open(&self) -> Result<duckdb::Connection> {
        let (Recipe::Confined(_, needs) | Recipe::Azure(_, needs)) = self;
        let needs: Vec<&str> = needs.as_deref().into_iter().collect();
        match self {
            Recipe::Confined(sandbox, _) => {
                crate::federation::open_duckdb_needing(sandbox, Some(ROOT_SCHEMA), &needs)
            }
            // Fetching the extension is allowed here: the source cannot be read
            // at all without it, and registering one is the consent.
            Recipe::Azure(access, _) => {
                crate::federation::open_duckdb_azure(access, Some(ROOT_SCHEMA), &needs)
            }
        }
    }
}

#[async_trait]
impl Connection for FilesConnection {
    /// A folder source has exactly one catalogue, and it is shown under the name
    /// of the folder itself rather than DuckDB's `memory` — the level exists so the
    /// explorer tree keeps its shape, and `parquet → public → customers` is what
    /// that shape is worth saying.
    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(vec![match &self.where_ {
            Place::Local { root, single } => crate::model::catalogue_name(
                if *single {
                    SourceKind::File
                } else {
                    SourceKind::Folder
                },
                root,
            ),
            // The container and how far into it, which is what identifies one
            // Azure source against another.
            Place::Azure { access, .. } => access.catalogue.clone(),
        }])
    }

    async fn list_tables(&self, _db: &str) -> Result<Vec<TableInfo>> {
        Ok(self
            .sets()
            .into_iter()
            .map(|set| TableInfo {
                schema: ROOT_SCHEMA.to_owned(),
                name: set.name,
                // A view is what it is in DuckDB, and it says "this is a file,
                // not something you can write to".
                kind: TableKind::View,
            })
            .collect())
    }

    async fn list_columns(&self, _db: &str, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let set = self
            .sets()
            .into_iter()
            .find(|set| schema == ROOT_SCHEMA && set.name == table)
            .ok_or_else(|| {
                Error::BadRequest(format!("no table `{schema}.{table}` in this source"))
            })?;
        let session = self.recipe();
        let options = self.options.clone();

        tokio::task::spawn_blocking(move || {
            let connection = session.open()?;
            describe(&connection, &set, &options)
        })
        .await
        .map_err(|e| Error::Federated(format!("reading the file failed: {e}")))?
    }

    async fn snapshot(&self, _db: &str) -> Result<Vec<TableSchema>> {
        let sets = self.sets();
        let session = self.recipe();
        let options = self.options.clone();

        tokio::task::spawn_blocking(move || {
            let connection = session.open()?;
            let mut tables = Vec::new();
            for set in &sets {
                // One unreadable table must not cost you the schema of the rest.
                match describe(&connection, set, &options) {
                    Ok(columns) => tables.push(TableSchema {
                        schema: ROOT_SCHEMA.to_owned(),
                        name: set.name.clone(),
                        kind: TableKind::View,
                        columns,
                    }),
                    Err(e) => {
                        tracing::warn!(table = %set.label(), error = %e, "skipped in snapshot")
                    }
                }
            }
            Ok(tables)
        })
        .await
        .map_err(|e| Error::Federated(format!("reading the folder failed: {e}")))?
    }

    fn execute<'a>(&'a self, sql: &'a str) -> BoxStream<'a, Result<RowBatch>> {
        Box::pin(try_stream! {
            let sets = self.referenced(sql);
            let session = self.recipe();
            let options = self.options.clone();
            let sql = sql.to_owned();
            let (sink, mut source) = tokio::sync::mpsc::channel::<Result<RowBatch>>(4);

            // DuckDB is synchronous, so it gets its own thread rather than
            // stalling the runtime for the length of the query.
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                let connection = session.open()?;
                define(&connection, &sets, &options)?;
                federation::run(&connection, &sql, &sink)
            });

            while let Some(batch) = source.recv().await {
                yield batch?;
            }
            worker
                .await
                .map_err(|e| Error::Federated(format!("the query panicked: {e}")))?
                .map_err(|e| self.explain(e))?;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sales")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("customers.csv"), "id,name\n1,ada\n").unwrap();
        std::fs::write(root.join("notes.md"), "ignored").unwrap();
        std::fs::write(root.join("sales/orders.csv"), "id\n1\n").unwrap();
        std::fs::write(root.join("sales/refunds.csv"), "id\n2\n").unwrap();
        std::fs::write(root.join("node_modules/dep.csv"), "id\n9\n").unwrap();
        dir
    }

    fn csv() -> FileOptions {
        FileOptions {
            format: Some(FileFormat::Csv),
            ..FileOptions::default()
        }
    }

    #[test]
    fn root_files_are_their_own_tables_and_a_subdirectory_is_one() {
        let dir = fixture();
        let sets = walk(dir.path(), FileFormat::Csv, &csv());

        let found: Vec<(String, usize, bool)> = sets
            .iter()
            .map(|set| (set.name.clone(), set.files.len(), set.tagged))
            .collect();
        assert_eq!(
            found,
            [
                // The root's own file, under its own name and with no file column.
                ("customers".to_owned(), 1, false),
                // The subdirectory, over both its files.
                ("sales".to_owned(), 2, true),
            ],
            "node_modules should not be indexed, and .md is not a data file"
        );
    }

    /// A folder handed straight to [`FilesConnection::over`], without a path from
    /// a person to check first — the shape the Azure source used to take, and the
    /// one `@import` still does.
    #[test]
    fn a_folder_given_directly_reads_as_a_folder_source() {
        let dir = fixture();
        let over = FilesConnection::over(dir.path().to_path_buf(), csv());

        let found: Vec<(String, usize, bool)> = over
            .sets()
            .iter()
            .map(|set| (set.name.clone(), set.files.len(), set.tagged))
            .collect();
        assert_eq!(
            found,
            [("customers".to_owned(), 1, false), ("sales".to_owned(), 2, true)]
        );
        // A folder, never a single file: the sandbox has to grant the directory,
        // or the union across `sales` could not be read.
        assert!(matches!(
            over.where_,
            Place::Local { single: false, .. }
        ));
    }

    /// A container in Azure becomes the same tables a folder of the same shape
    /// would, from a listing alone — no file is opened to work that out.
    #[test]
    fn a_listing_becomes_the_same_tables_a_folder_would() {
        let listing = vec![
            (
                "customers.csv".to_owned(),
                "abfss://fs@acct.dfs.core.windows.net/customers.csv".to_owned(),
            ),
            (
                "sales/orders.csv".to_owned(),
                "abfss://fs@acct.dfs.core.windows.net/sales/orders.csv".to_owned(),
            ),
            (
                "sales/refunds.csv".to_owned(),
                "abfss://fs@acct.dfs.core.windows.net/sales/refunds.csv".to_owned(),
            ),
        ];
        let azure = FilesConnection::in_azure(
            crate::federation::AzureAccess {
                account: "acct".to_owned(),
                token: "t".to_owned(),
                catalogue: "fs".to_owned(),
            },
            &listing,
            FileFormat::Csv,
            csv(),
        );

        let found: Vec<(String, usize, bool)> = azure
            .sets()
            .iter()
            .map(|set| (set.name.clone(), set.files.len(), set.tagged))
            .collect();
        assert_eq!(
            found,
            [("customers".to_owned(), 1, false), ("sales".to_owned(), 2, true)],
            "the root's file is its own table, the subdirectory is one over both"
        );

        // And the SQL reads the URLs where they are, rather than anything local.
        let sales = azure.sets().into_iter().find(|s| s.name == "sales").unwrap();
        let query = sales.query(&csv()).unwrap();
        assert!(query.contains("abfss://fs@acct.dfs.core.windows.net/sales/orders.csv"), "{query}");
        assert!(query.contains("read_csv"), "{query}");
    }

    /// A root file and a subdirectory of the same name both survive.
    #[test]
    fn a_subdirectory_does_not_shadow_a_root_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sales")).unwrap();
        std::fs::write(dir.path().join("sales.csv"), "id\n1\n").unwrap();
        std::fs::write(dir.path().join("sales/2024.csv"), "id\n2\n").unwrap();

        let names: Vec<String> = walk(dir.path(), FileFormat::Csv, &csv())
            .into_iter()
            .map(|set| set.name)
            .collect();
        assert_eq!(names, ["sales", "sales_2"], "neither may vanish");
    }

    #[test]
    fn the_declared_format_is_the_only_one_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sales.csv"), "id\n1\n").unwrap();
        std::fs::write(dir.path().join("sales.json"), "[{\"id\":1}]").unwrap();

        let sets = walk(dir.path(), FileFormat::Json, &csv());
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].files.len(), 1, "only the JSON: {:?}", sets[0].files);
    }

    #[test]
    fn excluded_files_are_left_out_of_their_table() {
        let dir = fixture();
        let options = FileOptions {
            exclude: vec!["sales/orders.csv".to_owned()],
            ..csv()
        };
        let sets = walk(dir.path(), FileFormat::Csv, &options);

        let sales = sets.iter().find(|set| set.name == "sales").unwrap();
        assert_eq!(sales.files.len(), 1, "{:?}", sales.files);
        assert!(sales.files[0].ends_with("refunds.csv"));
        // Everything else is untouched: this excludes a file, not a directory.
        assert!(sets.iter().any(|set| set.name == "customers"));
    }

    /// Excluding every file of a directory takes the table with it, rather than
    /// leaving an empty one behind.
    #[test]
    fn a_directory_emptied_by_exclusions_is_not_a_table() {
        let dir = fixture();
        let options = FileOptions {
            exclude: vec![
                "sales/orders.csv".to_owned(),
                "sales/refunds.csv".to_owned(),
            ],
            ..csv()
        };
        let names: Vec<String> = walk(dir.path(), FileFormat::Csv, &options)
            .into_iter()
            .map(|set| set.name)
            .collect();
        assert_eq!(names, ["customers"]);
    }

    #[test]
    fn the_union_carries_the_file_name_and_a_single_file_does_not() {
        let dir = fixture();
        let sets = walk(dir.path(), FileFormat::Csv, &csv());
        let sales = sets.iter().find(|set| set.name == "sales").unwrap();
        let query = sales.query(&csv()).unwrap();

        assert!(query.contains("filename = true"), "{query}");
        assert!(query.contains("AS \"source_file\""), "{query}");
        assert!(query.contains("orders.csv"), "{query}");
        assert!(query.contains("refunds.csv"), "{query}");

        let one = DataSet {
            name: "customers".to_owned(),
            files: vec![address_of(&dir.path().join("customers.csv"))],
            format: FileFormat::Csv,
            sheet: None,
            tagged: false,
        };
        let query = one.query(&csv()).unwrap();
        assert!(!query.contains("filename"), "a file source: {query}");

        // A file at the root is a named file, so it gets no file column either.
        let root_file = sets.iter().find(|set| set.name == "customers").unwrap();
        let query = root_file.query(&csv()).unwrap();
        assert!(!query.contains("filename"), "a root file: {query}");
    }

    /// Including the excluded ones: a list that hid them could never put them back.
    #[test]
    fn candidates_lists_every_readable_file_relatively() {
        let dir = fixture();
        let options = FileOptions {
            exclude: vec!["sales/orders.csv".to_owned()],
            ..csv()
        };
        assert_eq!(
            candidates(dir.path(), &options),
            [
                "customers.csv".to_owned(),
                "sales/orders.csv".to_owned(),
                "sales/refunds.csv".to_owned(),
            ]
        );
    }

    #[test]
    fn only_named_tables_are_defined() {
        let dir = fixture();
        let connection = FilesConnection::over(dir.path().to_path_buf(), csv());
        let referenced = connection.referenced("select * from sales");
        assert_eq!(referenced.len(), 1);
        assert_eq!(referenced[0].name, "sales");
        assert!(connection.referenced("select 1").is_empty());
    }
}
