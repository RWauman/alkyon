//! The open folder: which directory the `.sql` tree, the file API and the
//! terminal's working directory all refer to.
//!
//! Deliberately server-side. The browser's `showDirectoryPicker()` hands back a
//! handle with a name and no path, so a folder picked in the page could never
//! tell the shell where to start — and under Docker the files live next to the
//! server anyway, not on the laptop holding the browser.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// Only `.sql` files are listed, read or written. The workspace API is not a
/// general-purpose file primitive — that matters as soon as `ALKYON_BIND` is
/// pointed at anything but loopback.
const EXTENSION: &str = "sql";

/// Directories never worth walking in a SQL project. Shared with folder sources,
/// which have no more reason to index `node_modules` than this does.
pub(crate) const SKIP: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".vs",
];

const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Relative to the root, always with `/` separators so the UI can treat the
    /// value as an opaque key on every platform.
    pub path: String,
    pub name: String,
    pub bytes: u64,
}

/// Every `.sql` file under `root`, shallowest first then alphabetical.
///
/// Truncated rather than unbounded: `truncated` is true when a real project would
/// have flooded the sidebar, and the UI says so instead of pretending the list is
/// complete.
pub fn list(root: &Path) -> Result<(Vec<Entry>, bool)> {
    let mut entries = Vec::new();
    let mut truncated = false;
    walk(root, root, 0, &mut entries, &mut truncated)?;
    entries.sort_by(|a, b| {
        let depth = |e: &Entry| e.path.matches('/').count();
        depth(a).cmp(&depth(b)).then_with(|| a.path.cmp(&b.path))
    });
    Ok((entries, truncated))
}

fn walk(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut Vec<Entry>,
    truncated: &mut bool,
) -> Result<()> {
    if depth > MAX_DEPTH {
        *truncated = true;
        return Ok(());
    }

    // An unreadable subdirectory should not fail the whole listing.
    let Ok(children) = std::fs::read_dir(directory) else {
        return Ok(());
    };

    for child in children.flatten() {
        if entries.len() >= MAX_FILES {
            *truncated = true;
            return Ok(());
        }
        let path = child.path();
        let name = child.file_name().to_string_lossy().into_owned();

        let Ok(kind) = child.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, entries, truncated)?;
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
        {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            entries.push(Entry {
                path: relative.to_string_lossy().replace('\\', "/"),
                name,
                bytes: child.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    Ok(())
}

/// Turn a client-supplied relative path into an absolute one inside `root`.
///
/// Refuses anything that is not a plain relative `.sql` path, then confirms
/// through the filesystem that the result really sits under the root — the string
/// check alone would miss a symlink pointing outside.
pub fn resolve(root: &Path, relative: &str, must_exist: bool) -> Result<PathBuf> {
    let candidate = Path::new(relative);

    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::BadRequest(format!(
            "`{relative}` must be a plain relative path inside the workspace"
        )));
    }
    if !candidate
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
    {
        return Err(Error::BadRequest(format!(
            "`{relative}` is not a .sql file"
        )));
    }

    let root = root
        .canonicalize()
        .map_err(|e| Error::BadRequest(format!("workspace root is unreadable: {e}")))?;
    let joined = root.join(candidate);

    // A file being created does not exist yet, so anchor on its parent instead.
    let anchor = if must_exist {
        joined.clone()
    } else {
        joined
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.clone())
    };
    let real = anchor.canonicalize().map_err(|e| {
        Error::BadRequest(format!("cannot resolve `{relative}` in the workspace: {e}"))
    })?;
    if !real.starts_with(&root) {
        return Err(Error::BadRequest(format!(
            "`{relative}` escapes the workspace"
        )));
    }

    Ok(joined)
}

/// Drop a leading UTF-8 byte-order mark.
///
/// SSMS writes `.sql` files with one by default. Left in place it becomes an
/// invisible first character of the first statement, and both PostgreSQL and SQL
/// Server reject that with a baffling syntax error. Files are written back
/// without a BOM.
pub fn strip_bom(mut text: String) -> String {
    const BOM: char = '\u{feff}';
    if text.starts_with(BOM) {
        text.drain(..BOM.len_utf8());
    }
    text
}

/// Turn a path a person typed into one the OS agrees exists: `~` expanded,
/// symlinks followed, and readable as a plain path.
///
/// Shared with folder and file *sources*, which point at a path the same way the
/// workspace does but may name a file rather than a directory.
pub fn resolve_root(path: &str) -> Result<PathBuf> {
    let expanded = expand_home(path);
    let resolved = Path::new(&expanded)
        .canonicalize()
        .map_err(|e| Error::BadRequest(format!("cannot open `{path}`: {e}")))?;
    Ok(simplify(resolved))
}

/// Accept a folder as the workspace root, rejecting anything that is not a
/// readable directory.
pub fn open(path: &str) -> Result<PathBuf> {
    let root = resolve_root(path)?;
    if !root.is_dir() {
        return Err(Error::BadRequest(format!("`{path}` is not a directory")));
    }
    Ok(root)
}

/// `canonicalize` hands back Windows verbatim paths (`\\?\C:\…`). They work for
/// file access but make a poor working directory — PowerShell renders one as a
/// provider-qualified monstrosity — and they read badly in the UI.
fn simplify(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = path.to_string_lossy().strip_prefix(r"\\?\") {
            // A real UNC path (`\\?\UNC\server\share`) has no equally simple
            // form, so leave those alone.
            let drive_letter = rest.as_bytes().get(1) == Some(&b':');
            if drive_letter && !rest.starts_with("UNC\\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// `~` is what a person types; it is not a path the OS understands.
///
/// Only bare `~` and `~/…` / `~\…` expand. `~alice` is left alone: on Unix that
/// means another user's home, and resolving it to `$HOME/alice` would be a
/// confidently wrong answer rather than an honest failure.
fn expand_home(path: &str) -> String {
    let rest = match path {
        "~" => "",
        p if p.starts_with("~/") || p.starts_with("~\\") => &p[2..],
        _ => return path.to_owned(),
    };
    let Some(dirs) = directories::UserDirs::new() else {
        return path.to_owned();
    };
    if rest.is_empty() {
        return dirs.home_dir().to_string_lossy().into_owned();
    }
    // `join` is what puts the separator back — concatenating strings here silently
    // produced `C:\Users\nameproject\…`.
    dirs.home_dir().join(rest).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("reports/monthly")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("top.sql"), "select 1").unwrap();
        std::fs::write(root.join("notes.txt"), "ignored").unwrap();
        std::fs::write(root.join("reports/a.sql"), "select 2").unwrap();
        std::fs::write(root.join("reports/monthly/b.SQL"), "select 3").unwrap();
        std::fs::write(root.join(".git/hook.sql"), "hidden").unwrap();
        std::fs::write(root.join("node_modules/pkg/dep.sql"), "vendored").unwrap();
        dir
    }

    #[test]
    fn lists_only_sql_and_skips_noise() {
        let dir = fixture();
        let (entries, truncated) = list(dir.path()).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();

        assert_eq!(paths, ["top.sql", "reports/a.sql", "reports/monthly/b.SQL"]);
        assert!(!truncated);
    }

    #[test]
    fn resolves_inside_the_root() {
        let dir = fixture();
        let resolved = resolve(dir.path(), "reports/a.sql", true).unwrap();
        assert!(resolved.ends_with("a.sql"));
    }

    #[test]
    fn refuses_paths_that_leave_the_root() {
        let dir = fixture();
        for attempt in [
            "../outside.sql",
            "reports/../../outside.sql",
            "/etc/passwd.sql",
            "reports/./../../x.sql",
        ] {
            assert!(
                resolve(dir.path(), attempt, false).is_err(),
                "{attempt} should have been refused"
            );
        }
    }

    #[test]
    fn refuses_anything_but_sql() {
        let dir = fixture();
        assert!(resolve(dir.path(), "notes.txt", true).is_err());
        assert!(resolve(dir.path(), "top", true).is_err());
    }

    #[test]
    fn a_new_file_resolves_against_its_parent() {
        let dir = fixture();
        let resolved = resolve(dir.path(), "reports/fresh.sql", false).unwrap();
        assert!(resolved.ends_with("fresh.sql"));
        // But only where the parent itself is inside the workspace.
        assert!(resolve(dir.path(), "nowhere/fresh.sql", false).is_err());
    }

    #[test]
    fn a_byte_order_mark_never_reaches_the_engine() {
        assert_eq!(strip_bom("\u{feff}select 1".to_owned()), "select 1");
        // Only the leading one, and only if there is one.
        assert_eq!(strip_bom("select 1".to_owned()), "select 1");
        assert_eq!(strip_bom(String::new()), "");
        assert_eq!(
            strip_bom("select '\u{feff}' as bom".to_owned()),
            "select '\u{feff}' as bom"
        );
    }

    #[test]
    fn a_tilde_expands_with_its_separator_intact() {
        let home = directories::UserDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();

        assert_eq!(Path::new(&expand_home("~")), home);
        for spelling in ["~/sub/deep", "~\\sub\\deep"] {
            let expanded = PathBuf::from(expand_home(spelling));
            assert!(
                expanded.starts_with(&home) && expanded.ends_with("deep"),
                "{spelling} expanded to {}",
                expanded.display()
            );
            // The bug this guards: home and the remainder run together.
            assert_ne!(
                expanded,
                PathBuf::from(format!("{}sub", home.to_string_lossy())),
                "{spelling} lost its separator"
            );
        }

        // Not ours to interpret.
        assert_eq!(expand_home("~alice/x"), "~alice/x");
        assert_eq!(expand_home("C:\\plain"), "C:\\plain");
    }

    #[test]
    fn the_root_is_a_path_a_shell_can_use() {
        let dir = fixture();
        let root = open(dir.path().to_str().unwrap()).unwrap();
        assert!(
            !root.to_string_lossy().starts_with(r"\\?\"),
            "a verbatim path is a bad working directory: {}",
            root.display()
        );
        // Still has to resolve files afterwards.
        assert!(resolve(&root, "top.sql", true).is_ok());
    }

    #[test]
    fn opening_a_file_as_a_root_fails() {
        let dir = fixture();
        let file = dir.path().join("top.sql");
        assert!(open(file.to_str().unwrap()).is_err());
        assert!(open(dir.path().to_str().unwrap()).is_ok());
    }
}
