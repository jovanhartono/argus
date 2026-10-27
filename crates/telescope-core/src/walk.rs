//! The first scan: reads every folder under a root, on all cores at once.

use crate::Ignore;
use crate::index::{EntryId, Index, IndexBuilder, NO_PARENT};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Scan {
    pub index: Index,
    /// Folders not read in full: no permission (Mail without Full Disk Access),
    /// deleted while the scan ran, or a disk or network error.
    pub unreadable_folders: usize,
}

/// Hidden folders, ~/Library bar its cloud drives, the insides of packages like Photos Library,
/// and tool downloads and caches hold most of a home folder's entries, and almost nobody
/// searches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clutter {
    Skip,
    Keep,
}

struct Walk {
    shared: Mutex<Shared>,
    clutter: Clutter,
    ignore: Ignore,
    root: EntryId,
    home: PathBuf,
    library: PathBuf,
}

struct Shared {
    builder: IndexBuilder,
    unreadable_folders: usize,
}

/// Symbolic links are listed but never followed. Your ignore list counts only when clutter
/// is skipped.
pub fn scan(root: &Path, clutter: Clutter, ignore: &Ignore) -> io::Result<Scan> {
    scan_from(root, clutter, ignore, home())
}

/// The real home folder, the way the scan reports paths, or empty when there is none.
pub fn home() -> PathBuf {
    std::env::home_dir()
        .and_then(|home| fs::canonicalize(home).ok())
        .unwrap_or_default()
}

fn scan_from(root: &Path, clutter: Clutter, ignore: &Ignore, home: PathBuf) -> io::Result<Scan> {
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
        ignore: ignore.clone(),
        root: root_id,
        library: home.join("Library"),
        home,
    };
    rayon::scope(|scope| read_folder(scope, root, root_id, false, &walk));

    let shared = walk.shared.into_inner().unwrap();
    Ok(Scan {
        index: shared.builder.finish(),
        unreadable_folders: shared.unreadable_folders,
    })
}

fn is_clutter(folder: &Path, name: &OsStr, home: &Path) -> bool {
    name.as_encoded_bytes().starts_with(b".")
        || name == "node_modules"
        || name == "__pycache__"
        || (name == "mod" && folder.strip_prefix(home) == Ok(Path::new("go/pkg")))
}

/// Downloads a tool keeps beside its project, known by the project file next to them.
/// A folder that happens to be called "vendor" is kept.
fn is_tool_download(name: &OsStr, listing: &[(OsString, bool)]) -> bool {
    let beside = |file: &str| listing.iter().any(|(name, _)| name == file);
    match name.to_str() {
        Some("vendor") => beside("composer.json") || beside("go.mod"),
        Some("Pods") => beside("Podfile"),
        // A Python environment, even one made inside a project with `python -m venv .`.
        Some("bin" | "include" | "Include" | "lib" | "Lib" | "lib64" | "Scripts") => {
            beside("pyvenv.cfg")
        }
        _ => false,
    }
}

/// Finder shows these folders as one file, like the thousands of pieces inside Photos Library.
const PACKAGES: &[&str] = &[
    "aplibrary",
    "fcpbundle",
    "imovielibrary",
    "logicx",
    "lrlibrary",
    "musiclibrary",
    "photolibrary",
    "photoslibrary",
    "pvm",
    "rtfd",
    "sparsebundle",
    "tvlibrary",
    "utm",
    "vmwarevm",
    "xcarchive",
    "xcodeproj",
    "xcworkspace",
];

/// Packages, and folders a tool labelled as its cache, are listed in their parent with nothing
/// inside them.
fn is_sealed(path: &Path, listing: &[(OsString, bool)]) -> bool {
    let has = |file: &str| listing.iter().any(|(name, _)| name == file);
    let extension = path.extension().and_then(OsStr::to_str);
    match extension.map(str::to_ascii_lowercase).as_deref() {
        // Ordinary folders get these names too, like a project called storefront.app.
        Some("app" | "appex" | "bundle" | "framework" | "plugin") => {
            has("Contents") || has("Versions") || has("Info.plist")
        }
        Some("key" | "numbers" | "pages") => has("Index.zip") || has("Index"),
        Some("band") => has("projectData") || has("Alternatives"),
        Some("playground") => has("contents.xcplayground"),
        Some(extension) if PACKAGES.contains(&extension) => true,
        _ => has("CACHEDIR.TAG") && is_cache_tag(&path.join("CACHEDIR.TAG")),
    }
}

/// The cache-tag standard Rust, pytest and others follow: a file that starts with this line.
fn is_cache_tag(file: &Path) -> bool {
    let mut start = [0; 43];
    let read = fs::File::open(file).and_then(|mut file| file.read_exact(&mut start));
    read.is_ok() && start == *b"Signature: 8a477f597d28d172789f06886806bc55"
}

/// Each job carries its folder's number, so no path-to-number table is needed.
/// A `passage` folder is one the rules skip, read only on the way to one you brought back.
fn read_folder<'s>(
    scope: &rayon::Scope<'s>,
    path: PathBuf,
    folder: EntryId,
    passage: bool,
    walk: &'s Walk,
) {
    let shared = &walk.shared;
    let Ok(entries) = fs::read_dir(&path) else {
        shared.lock().unwrap().unreadable_folders += 1;
        return;
    };
    let mut listing = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            shared.lock().unwrap().unreadable_folders += 1;
            break;
        };
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        listing.push((entry.file_name(), is_dir));
    }
    // What you scan is always read, whatever it is.
    let skip = walk.clutter == Clutter::Skip && folder != walk.root;
    let name = path.file_name().unwrap_or_default();
    let kept = walk.ignore.keeps(&path, name);
    let sealed = skip && !passage && !kept && is_sealed(&path, &listing);
    if sealed && !walk.ignore.leads_to_kept(&path) {
        return;
    }
    // Only cloud drives are kept from ~/Library: iCloud Drive, Dropbox, Google Drive, OneDrive.
    let in_library = skip && !kept && path == walk.library;
    let passage = passage || sealed || in_library;

    // Names are gathered first so the shared lock is taken once per folder, not once per file.
    let mut names = String::new();
    let mut children: Vec<(usize, bool)> = Vec::new();
    let mut subfolders = Vec::new();
    for (name, is_dir) in &listing {
        let is_dir = *is_dir;
        let child_path = is_dir.then(|| path.join(name));
        let mut child_passage = false;
        if walk.clutter == Clutter::Skip {
            match &child_path {
                None if passage => continue,
                None => {}
                Some(child) => {
                    let cloud =
                        in_library && (name == "Mobile Documents" || name == "CloudStorage");
                    let wanted = walk.ignore.keeps(child, name)
                        || (cloud && !walk.ignore.skips(child, name));
                    let dropped = passage
                        || is_clutter(&path, name, &walk.home)
                        || is_tool_download(name, &listing)
                        || walk.ignore.skips(child, name);
                    if !wanted && dropped {
                        // A skipped folder on the way to one you brought back is walked
                        // through, keeping only the way there.
                        if !walk.ignore.leads_to_kept(child) {
                            continue;
                        }
                        child_passage = true;
                    }
                }
            }
        }
        names.push_str(&name.to_string_lossy());
        children.push((names.len(), is_dir));
        subfolders.extend(child_path.map(|child| (child, child_passage)));
    }

    let mut subfolder_ids = Vec::with_capacity(subfolders.len());
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

    for (child, (child_path, passage)) in subfolder_ids.into_iter().zip(subfolders) {
        scope.spawn(move |scope| read_folder(scope, child_path, child, passage, walk));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn scan(root: &Path, clutter: Clutter) -> io::Result<Scan> {
        super::scan(root, clutter, &Ignore::default())
    }

    fn scan_from(root: &Path, clutter: Clutter, home: PathBuf) -> io::Result<Scan> {
        super::scan_from(root, clutter, &Ignore::default(), home)
    }

    fn paths_below(root: &Path, index: &Index) -> BTreeSet<PathBuf> {
        (1..index.len() as EntryId)
            .map(|entry| index.path(entry).strip_prefix(root).unwrap().to_path_buf())
            .collect()
    }

    #[test]
    fn your_ignore_list_skips_folders_and_brings_others_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = &fs::canonicalize(dir.path()).unwrap();
        for file in [
            "Old Projects/a.txt",
            "work/Old Projects/b.txt",
            "Movies/Exports/c.mov",
            "Movies/Clips/d.mov",
            ".config/e.toml",
            ".cache/f",
            ".cache/other/g",
            ".cache/keep/h.txt",
            "Trip.photoslibrary/originals/i.heic",
            "Old.photoslibrary/originals/j.heic",
            "Old.photoslibrary/resources/k.jpeg",
        ] {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::write(root.join(file), "").unwrap();
        }
        let rules = "old projects\n~/Movies/Exports\n!~/.config\n!~/.cache/keep\n\
                     !~/Trip.photoslibrary\n!~/Old.photoslibrary/originals\n";
        let (ignore, problems) = Ignore::parse(rules, root);
        assert!(problems.is_empty());

        let index = super::scan_from(root, Clutter::Skip, &ignore, PathBuf::new()).unwrap();
        let expected: BTreeSet<PathBuf> = [
            "work",
            "Movies",
            "Movies/Clips",
            "Movies/Clips/d.mov",
            ".config",
            ".config/e.toml",
            ".cache",
            ".cache/keep",
            ".cache/keep/h.txt",
            "Trip.photoslibrary",
            "Trip.photoslibrary/originals",
            "Trip.photoslibrary/originals/i.heic",
            "Old.photoslibrary",
            "Old.photoslibrary/originals",
            "Old.photoslibrary/originals/j.heic",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(paths_below(root, &index.index), expected);
    }

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
    fn lists_packages_and_tool_folders_without_their_insides() {
        let dir = tempfile::tempdir().unwrap();
        let root = &fs::canonicalize(dir.path()).unwrap();
        let files = [
            "Photos Library.photoslibrary/resources/a.jpeg",
            "Notes.app/Contents/Info.plist",
            "storefront.app/src/main.ts",
            "site/vendor/lib.php",
            "site/composer.json",
            "notes/vendor/contract.pdf",
            "notes/CACHEDIR.TAG",
            "app/Pods/Alamofire/a.swift",
            "app/Podfile",
            "rust/target/CACHEDIR.TAG",
            "rust/target/debug/tl",
            "py/pyvenv.cfg",
            "py/lib/site.py",
            "py/main.py",
        ];
        for file in files {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::write(root.join(file), "").unwrap();
        }
        let tag = "Signature: 8a477f597d28d172789f06886806bc55\n";
        fs::write(root.join("rust/target/CACHEDIR.TAG"), tag).unwrap();

        let index = scan_from(root, Clutter::Skip, PathBuf::new())
            .unwrap()
            .index;
        let found: BTreeSet<PathBuf> = (1..index.len() as EntryId)
            .map(|entry| index.path(entry).strip_prefix(root).unwrap().to_path_buf())
            .collect();
        let expected: BTreeSet<PathBuf> = [
            "Photos Library.photoslibrary",
            "Notes.app",
            "storefront.app",
            "storefront.app/src",
            "storefront.app/src/main.ts",
            "site",
            "site/composer.json",
            "notes",
            "notes/vendor",
            "notes/vendor/contract.pdf",
            "notes/CACHEDIR.TAG",
            "app",
            "app/Podfile",
            "rust",
            "rust/target",
            "py",
            "py/pyvenv.cfg",
            "py/main.py",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(found, expected);
        let everything = scan_from(root, Clutter::Keep, PathBuf::new()).unwrap();
        assert_eq!(everything.index.len(), 33);
    }

    #[test]
    fn keeps_only_cloud_drives_from_the_home_library() {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        for file in [
            "Library/Caches/a.db",
            "Library/notes.txt",
            "Library/Mobile Documents/com~apple~CloudDocs/cv.pdf",
            "Library/CloudStorage/Dropbox/plan.md",
            "Library/Application Support/Foo/foo.db",
            "Library/Application Support/Bar/bar.db",
            "Documents/Library/book.pdf",
        ] {
            fs::create_dir_all(home.join(file).parent().unwrap()).unwrap();
            fs::write(home.join(file), "").unwrap();
        }
        let names_with = |root: &Path, rules: &str| {
            let (ignore, _) = Ignore::parse(rules, &home);
            let index = super::scan_from(root, Clutter::Skip, &ignore, home.clone());
            let index = index.unwrap().index;
            let mut names: Vec<String> = (1..index.len() as EntryId)
                .map(|entry| index.name(entry).to_owned())
                .collect();
            names.sort();
            names
        };
        let names = |root: &Path| names_with(root, "");

        let found = names(&home);
        assert!(found.contains(&"cv.pdf".to_owned()));
        assert!(found.contains(&"plan.md".to_owned()));
        assert!(found.contains(&"book.pdf".to_owned()));
        assert!(!found.contains(&"a.db".to_owned()));
        assert!(!found.contains(&"notes.txt".to_owned()));
        assert!(names(&home.join("Library")).contains(&"a.db".to_owned()));

        let found = names_with(&home, "!~/Library/Application Support/Foo");
        assert!(found.contains(&"foo.db".to_owned()));
        assert!(found.contains(&"cv.pdf".to_owned()));
        assert!(!found.contains(&"bar.db".to_owned()));
        assert!(!found.contains(&"a.db".to_owned()));
        assert!(names_with(&home, "!~/Library").contains(&"a.db".to_owned()));
        assert!(!names_with(&home, "CloudStorage").contains(&"plan.md".to_owned()));
    }

    #[test]
    fn go_downloads_are_clutter_only_in_the_home_folder() {
        let home = Path::new("/Users/ana");
        assert!(is_clutter(&home.join("go/pkg"), OsStr::new("mod"), home));
        assert!(!is_clutter(&home.join("work/pkg"), OsStr::new("mod"), home));
    }
}
