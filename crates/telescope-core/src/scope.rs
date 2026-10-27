//! Which changed folders call for a new list, and which folders to watch for changes.

use crate::index::{EntryId, Index};
use crate::walk::{Inside, Rules};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The folders a list holds, judged by the rules of the scan that built it.
#[derive(Clone, Debug)]
pub struct Scope {
    root: PathBuf,
    rules: Rules,
}

impl Scope {
    pub(crate) fn new(root: PathBuf, rules: Rules) -> Self {
        Self { root, rules }
    }

    /// Whether the scan reads what is inside `path`, a real path as macOS reports changes, so
    /// a change there can change the list. Walks the rules from the root, looking only for
    /// the few files that mark a cache, an app or a tool's downloads, never inside a guarded
    /// folder. A folder already deleted is judged by its name.
    pub fn in_scope(&self, path: &Path) -> bool {
        self.reach(path).is_some()
    }

    /// Whether `index`, built by these rules, already lists what a scan would keep from
    /// `path` now, so a change there needs no rebuild: a file saved over, or a hidden file
    /// replaced, as apps do to their settings in the home folder all day. Reads the folder's
    /// names once. Only right while no rebuild is under way, which may hold a newer list.
    pub fn holds(&self, path: &Path, index: &Index) -> bool {
        let (Some(inside), Ok(rest)) = (self.reach(path), path.strip_prefix(&self.root)) else {
            return true;
        };
        let names: Option<Vec<&str>> = rest.iter().map(OsStr::to_str).collect();
        let Some(folder) = names.and_then(|names| index.find_folder(names)) else {
            return false;
        };
        let Ok(entries) = fs::read_dir(path) else {
            return false;
        };
        let mut listing = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            listing.push((entry.file_name(), is_dir));
        }
        let has = |file: &str| listing.iter().any(|(name, _)| name == file);
        let guarded = self.rules.guarded_in(path);
        let mut kept: Vec<(String, bool)> = listing
            .iter()
            .filter(|&(name, is_dir)| {
                let child = is_dir.then(|| path.join(name));
                let keeps = self
                    .rules
                    .keeps(path, inside, guarded, name, child.as_deref(), has);
                keeps.is_some()
            })
            .map(|(name, is_dir)| (name.to_string_lossy().into_owned(), *is_dir))
            .collect();
        let mut listed: Vec<(String, bool)> = index
            .children(folder)
            .map(|entry| (index.name(entry).to_owned(), index.is_dir(entry)))
            .collect();
        kept.sort_unstable();
        listed.sort_unstable();
        kept == listed
    }

    fn reach(&self, path: &Path) -> Option<Inside> {
        let rest = path.strip_prefix(&self.root).ok()?;
        if self.rules.is_guarded(path) {
            return None;
        }
        let mut names = rest.components();
        let mut folder = self.root.clone();
        let mut top = true;
        let mut passage = false;
        loop {
            let has = |name: &str| fs::symlink_metadata(folder.join(name)).is_ok();
            let opening = self.rules.opening(&folder, top, passage)?;
            let inside = self.rules.inside(&folder, opening, passage, has)?;
            let name = match names.next() {
                None => return Some(inside),
                Some(Component::Normal(name)) => name,
                Some(_) => return None,
            };
            let child = folder.join(name);
            passage = self
                .rules
                .keeps_folder(&folder, inside, &child, name, has)?;
            folder = child;
            top = false;
        }
    }

    /// The folders to watch, nearest the root. A folder read only on the way to one kept
    /// inside it, such as ~/Library, is watched through those, so its caches churning wakes
    /// nothing. The root is left out: its own list of names is watched on its own.
    pub fn watched(&self, index: &Index) -> Vec<PathBuf> {
        let mut watched = Vec::new();
        let mut passages: Vec<EntryId> = vec![0];
        while let Some(folder) = passages.pop() {
            for child in index.child_folders(folder) {
                let path = index.path(child);
                match self.reach(&path) {
                    Some(inside) if inside.passage => passages.push(child),
                    Some(_) => watched.push(path),
                    None => {}
                }
            }
        }
        watched.sort();
        watched
    }
}

#[cfg(test)]
mod tests {
    use crate::{Access, Clutter, Ignore, Settings};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Every folder below holds `probe.txt`, so a folder's insides are read exactly when its
    /// probe is listed, or when it is a passage, which keeps no files.
    const FOLDERS: &[&str] = &[
        "work",
        "work/new project/src",
        "work/node_modules/react",
        "work/.git/objects",
        "work/__pycache__",
        "work/site/vendor/lib",
        "work/notes/vendor",
        "work/py/lib/site",
        "work/py/bin",
        "work/rust/target/debug",
        "work/Notes.app/Contents",
        "work/satudental.app/src",
        "work/Trip.photoslibrary/originals",
        "work/Talk.key/Data",
        "Old Projects/a",
        "Movies/Exports",
        "Movies/Clips",
        "go/pkg/mod/github.com",
        "src/mod",
        ".config/nvim/lua",
        ".config/gh",
        ".cache/keep",
        ".cache/other",
        "Library/Caches/com.apple.x",
        "Library/Application Support/Foo/data",
        "Library/Application Support/Bar",
        "Library/Mobile Documents/com~apple~CloudDocs/cv",
        "Library/CloudStorage/Dropbox/plans",
        "Library/Containers/com.x/Data",
        "Documents/taxes",
        "Desktop/shots",
        "Downloads",
        "Music/Music/Media",
        "Pictures/Photos Library.photoslibrary/originals",
        "Pictures/holiday",
    ];

    const RULES: &str = "old projects\n~/Movies/Exports\n!~/.config/nvim\n!~/.cache/keep\n\
                         !~/Library/Application Support/Foo\n";

    fn home() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        for folder in FOLDERS {
            fs::create_dir_all(home.join(folder)).unwrap();
        }
        fs::write(home.join("probe.txt"), "").unwrap();
        for folder in FOLDERS {
            let mut path = home.join(folder);
            while path != home {
                fs::write(path.join("probe.txt"), "").unwrap();
                path.pop();
            }
        }
        for (file, text) in [
            ("work/site/composer.json", ""),
            ("work/py/pyvenv.cfg", ""),
            ("work/notes/CACHEDIR.TAG", "not a tag"),
            (
                "work/rust/target/CACHEDIR.TAG",
                "Signature: 8a477f597d28d172789f06886806bc55\n",
            ),
            ("work/Notes.app/Contents/Info.plist", ""),
            ("work/Talk.key/Index.zip", ""),
            (".claude.json", "{}"),
        ] {
            fs::write(home.join(file), text).unwrap();
        }
        (dir, home)
    }

    fn settings(home: &Path, access: Access) -> Settings {
        let (ignore, problems) = Ignore::parse(RULES, home, access);
        assert!(problems.is_empty());
        Settings::new(home, home, Clutter::Skip, ignore, access)
    }

    #[test]
    fn a_folder_is_in_scope_exactly_when_the_scan_reads_it() {
        let (_dir, home) = home();
        let passages = [
            ".config",
            ".cache",
            "Library",
            "Library/Application Support",
        ];
        for access in [Access::Full, Access::Limited] {
            let settings = settings(&home, access);
            let scope = settings.scope();
            let index = settings.scan().unwrap().index;
            let listed: Vec<PathBuf> = (0..index.len() as u32).map(|e| index.path(e)).collect();
            let mut folders: Vec<PathBuf> = vec![home.clone()];
            for folder in FOLDERS {
                let mut path = home.join(folder);
                while path != home {
                    folders.push(path.clone());
                    path.pop();
                }
            }
            folders.sort();
            folders.dedup();
            for folder in folders {
                let read = listed.contains(&folder.join("probe.txt"))
                    || passages.iter().any(|passage| folder == home.join(passage));
                assert_eq!(scope.in_scope(&folder), read, "{folder:?} with {access:?}");
            }
        }
    }

    #[test]
    fn spot_checks_of_what_counts() {
        let (_dir, home) = home();
        let full = settings(&home, Access::Full).scope();
        let limited = settings(&home, Access::Limited).scope();
        for scope in [&full, &limited] {
            assert!(scope.in_scope(&home));
            assert!(scope.in_scope(&home.join("work/new project/src")));
            // Deleted or not made yet: judged by name alone.
            assert!(scope.in_scope(&home.join("work/gone/deeper")));
            assert!(!scope.in_scope(&home.join("work/gone/node_modules/x")));
            assert!(!scope.in_scope(&home.join("work/rust/target/debug/deps")));
            assert!(!scope.in_scope(&home.join("Library/Caches/com.apple.x")));
            assert!(!scope.in_scope(Path::new("/elsewhere")));
            assert!(!scope.in_scope(home.parent().unwrap()));
            assert!(!scope.in_scope(&home.join("work/../Library/Caches")));
        }
        assert!(full.in_scope(&home.join("Documents/taxes")));
        assert!(full.in_scope(&home.join("Library/CloudStorage/Dropbox/plans")));
        assert!(!limited.in_scope(&home.join("Documents/taxes")));
        assert!(!limited.in_scope(&home.join("Library/CloudStorage/Dropbox/plans")));
        assert!(!limited.in_scope(&home.join("Pictures/Photos Library.photoslibrary")));
    }

    #[cfg(unix)]
    #[test]
    fn a_guarded_folder_is_never_looked_into() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, home) = home();
        let documents = home.join("Documents");
        // A look inside would find no CACHEDIR.TAG and call it in scope; a locked folder
        // stands in for the pop-up macOS would show.
        fs::set_permissions(&documents, fs::Permissions::from_mode(0o000)).unwrap();
        let limited = settings(&home, Access::Limited).scope();
        let inside = limited.in_scope(&documents.join("taxes"));
        fs::set_permissions(&documents, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!inside);
    }

    #[test]
    fn watches_kept_folders_and_skips_through_passages() {
        let (_dir, home) = home();
        let watched = |access| {
            let settings = settings(&home, access);
            let index = settings.scan().unwrap().index;
            settings.scope().watched(&index)
        };
        let paths = |names: &[&str]| -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = names.iter().map(|name| home.join(name)).collect();
            paths.sort();
            paths
        };
        assert_eq!(
            watched(Access::Full),
            paths(&[
                ".cache/keep",
                ".config/nvim",
                "Desktop",
                "Documents",
                "Downloads",
                "Library/Application Support/Foo",
                "Library/CloudStorage",
                "Library/Mobile Documents",
                "Movies",
                "Music",
                "Pictures",
                "go",
                "src",
                "work",
            ])
        );
        assert_eq!(
            watched(Access::Limited),
            paths(&[
                ".cache/keep",
                ".config/nvim",
                "Library/Application Support/Foo",
                "Movies",
                "Music",
                "Pictures",
                "go",
                "src",
                "work",
            ])
        );
    }

    fn every_folder(home: &Path) -> Vec<PathBuf> {
        let mut folders = vec![home.to_path_buf()];
        for folder in FOLDERS {
            let mut path = home.join(folder);
            while path != home {
                folders.push(path.clone());
                path.pop();
            }
        }
        folders.sort();
        folders.dedup();
        folders
    }

    #[test]
    fn a_fresh_list_holds_every_folder() {
        let (_dir, home) = home();
        for access in [Access::Full, Access::Limited] {
            let settings = settings(&home, access);
            let index = settings.scan().unwrap().index;
            let scope = settings.scope();
            for folder in every_folder(&home) {
                assert!(scope.holds(&folder, &index), "{folder:?} with {access:?}");
            }
            // As macOS reports folders, with a slash at the end.
            let slashed = PathBuf::from(format!("{}/", home.join("work").display()));
            assert!(scope.holds(&slashed, &index));
        }
    }

    #[test]
    fn only_a_change_in_kept_names_needs_a_rebuild() {
        let (_dir, home) = home();
        let settings = settings(&home, Access::Full);
        let index = settings.scan().unwrap().index;
        let scope = settings.scope();
        let holds = |folder: &str| scope.holds(&home.join(folder), &index);

        // Saved over, or a settings file replaced beside it: the same names.
        fs::write(home.join("work/probe.txt"), "edited").unwrap();
        fs::write(home.join(".claude.json.tmp"), "").unwrap();
        fs::rename(home.join(".claude.json.tmp"), home.join(".claude.json")).unwrap();
        fs::write(home.join("work/node_modules/react/index.js"), "").unwrap();
        fs::write(home.join("work/rust/target/debug/tl"), "").unwrap();
        fs::write(home.join(".config/gh/hosts.yml"), "").unwrap();
        fs::write(home.join(".config/settings.json"), "").unwrap();
        fs::create_dir(home.join(".config/fish")).unwrap();
        fs::create_dir(home.join("Library/Logs")).unwrap();
        fs::create_dir(home.join("work/.venv")).unwrap();
        for folder in [
            "",
            "work",
            "work/node_modules/react",
            "work/rust/target/debug",
            ".config",
            ".config/gh",
            "Library",
        ] {
            assert!(holds(folder), "{folder}");
        }

        fs::write(home.join("work/budget.txt"), "").unwrap();
        assert!(!holds("work"));
        fs::create_dir(home.join("Projects")).unwrap();
        assert!(!holds(""));
        fs::create_dir(home.join(".config/nvim/after")).unwrap();
        assert!(!holds(".config/nvim"));
        // Deleted, or not listed yet.
        fs::remove_dir_all(home.join("Movies/Clips")).unwrap();
        assert!(!holds("Movies/Clips"));
        assert!(!holds("Movies"));
        assert!(!holds("Projects"));
        // A folder turned into a file of the same name.
        fs::remove_dir_all(home.join("src/mod")).unwrap();
        fs::write(home.join("src/mod"), "").unwrap();
        assert!(!holds("src"));
    }

    #[test]
    fn without_full_disk_access_a_guarded_folder_is_held_unread() {
        let (_dir, home) = home();
        let settings = settings(&home, Access::Limited);
        let index = settings.scan().unwrap().index;
        let scope = settings.scope();
        fs::write(home.join("Documents/taxes/2026.pdf"), "").unwrap();
        assert!(scope.holds(&home.join("Documents/taxes"), &index));
        // Documents isn't listed in the home folder at all, so it stays out unchanged.
        fs::write(home.join("Documents/new.pdf"), "").unwrap();
        assert!(scope.holds(&home, &index));
    }

    #[test]
    fn a_sealed_top_folder_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(home.join("target/debug")).unwrap();
        let tag = "Signature: 8a477f597d28d172789f06886806bc55\n";
        fs::write(home.join("target/CACHEDIR.TAG"), tag).unwrap();
        fs::create_dir(home.join("notes")).unwrap();
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let index = settings.scan().unwrap().index;
        assert_eq!(settings.scope().watched(&index), [home.join("notes")]);
    }
}
