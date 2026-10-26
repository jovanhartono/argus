//! The first scan: reads every folder under a root, on all cores at once.

use crate::index::{EntryId, Index, IndexBuilder, NO_PARENT};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Scan {
    pub index: Index,
    /// Folders not read in full: no permission (Mail without Full Disk Access),
    /// deleted while the scan ran, or a disk or network error.
    pub unreadable_folders: usize,
}

/// Hidden folders, node_modules and ~/Library hold most of a home folder's entries,
/// and almost nobody searches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clutter {
    Skip,
    Keep,
}

struct Walk {
    shared: Mutex<Shared>,
    clutter: Clutter,
    home: PathBuf,
}

struct Shared {
    builder: IndexBuilder,
    unreadable_folders: usize,
}

/// Symbolic links are listed but never followed.
pub fn scan(root: &Path, clutter: Clutter) -> io::Result<Scan> {
    // The real path, as macOS reports file changes: /tmp becomes /private/tmp, no "..".
    let root = fs::canonicalize(root)?;
    if !fs::metadata(&root)?.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    // A root you may not open is an error, not an empty index.
    fs::read_dir(&root)?;

    let mut builder = IndexBuilder::new();
    let root_id = builder.push(&root.to_string_lossy(), NO_PARENT, true);
    let walk = Walk {
        shared: Mutex::new(Shared {
            builder,
            unreadable_folders: 0,
        }),
        clutter,
        home: std::env::home_dir()
            .and_then(|home| fs::canonicalize(home).ok())
            .unwrap_or_default(),
    };
    rayon::scope(|scope| read_folder(scope, root, root_id, &walk));

    let shared = walk.shared.into_inner().unwrap();
    Ok(Scan {
        index: shared.builder.finish(),
        unreadable_folders: shared.unreadable_folders,
    })
}

fn is_clutter(folder: &Path, name: &OsStr, home: &Path) -> bool {
    name.as_encoded_bytes().starts_with(b".")
        || name == "node_modules"
        || (name == "Library" && folder == home)
}

/// Each job carries its folder's number, so no path-to-number table is needed.
fn read_folder<'s>(scope: &rayon::Scope<'s>, path: PathBuf, folder: EntryId, walk: &'s Walk) {
    let shared = &walk.shared;
    let Ok(entries) = fs::read_dir(&path) else {
        shared.lock().unwrap().unreadable_folders += 1;
        return;
    };
    // Names are gathered first so the shared lock is taken once per folder, not once per file.
    let mut names = String::new();
    let mut children: Vec<(usize, bool)> = Vec::new();
    let mut subfolder_paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            shared.lock().unwrap().unreadable_folders += 1;
            break;
        };
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let name = entry.file_name();
        if is_dir && walk.clutter == Clutter::Skip && is_clutter(&path, &name, &walk.home) {
            continue;
        }
        names.push_str(&name.to_string_lossy());
        children.push((names.len(), is_dir));
        if is_dir {
            subfolder_paths.push(path.join(name));
        }
    }

    let mut subfolder_ids = Vec::with_capacity(subfolder_paths.len());
    let mut state = shared.lock().unwrap();
    let mut start = 0;
    for (end, is_dir) in children {
        let child = state.builder.push(&names[start..end], folder, is_dir);
        if is_dir {
            subfolder_ids.push(child);
        }
        start = end;
    }
    drop(state);

    for (child, child_path) in subfolder_ids.into_iter().zip(subfolder_paths) {
        scope.spawn(move |scope| read_folder(scope, child_path, child, walk));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn finds_every_file_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = &fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("src/empty")).unwrap();
        fs::write(root.join("docs/README.md"), "").unwrap();
        fs::write(root.join("src/README.md"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();

        let scan = scan(root, Clutter::Keep).unwrap();
        let index = &scan.index;
        let found: BTreeSet<PathBuf> = (0..index.len() as EntryId)
            .map(|entry| index.path(entry).strip_prefix(root).unwrap().to_path_buf())
            .collect();
        let expected: BTreeSet<PathBuf> = [
            "",
            "docs",
            "docs/README.md",
            "src",
            "src/README.md",
            "src/main.rs",
            "src/empty",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        assert_eq!(found, expected);
        assert_eq!(index.len(), 7);
        assert_eq!(index.distinct_names(), 6);
        assert_eq!(scan.unreadable_folders, 0);
    }

    #[cfg(unix)]
    #[test]
    fn lists_links_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("a")).unwrap();
        std::os::unix::fs::symlink("..", root.join("a/up")).unwrap();

        let scan = scan(root, Clutter::Keep).unwrap();
        let index = &scan.index;
        let up = (0..index.len() as EntryId)
            .find(|&entry| index.name(entry) == "up")
            .unwrap();

        assert_eq!(index.len(), 3);
        assert!(!index.is_dir(up));
    }

    #[cfg(unix)]
    #[test]
    fn counts_folders_it_may_not_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let scan = scan(dir.path(), Clutter::Keep).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(scan.index.len(), 2);
        assert_eq!(scan.unreadable_folders, 1);
    }

    #[test]
    fn rejects_a_file_as_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        fs::write(&file, "").unwrap();

        assert_eq!(
            scan(&file, Clutter::Keep).err().map(|e| e.kind()),
            Some(io::ErrorKind::NotADirectory)
        );
    }

    #[test]
    fn skips_clutter_unless_asked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for folder in [".git", "node_modules", "src", "src/.cache"] {
            fs::create_dir(root.join(folder)).unwrap();
        }
        fs::write(root.join(".env"), "").unwrap();

        assert_eq!(scan(root, Clutter::Skip).unwrap().index.len(), 3);
        assert_eq!(scan(root, Clutter::Keep).unwrap().index.len(), 6);
    }

    #[test]
    fn library_is_clutter_only_in_the_home_folder() {
        let home = Path::new("/Users/ana");
        assert!(is_clutter(home, OsStr::new("Library"), home));
        assert!(!is_clutter(
            &home.join("Documents"),
            OsStr::new("Library"),
            home
        ));
    }
}
