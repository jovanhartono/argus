//! The first scan: reads every folder under a root, on all cores at once.

use crate::Ignore;
use crate::index::{EntryId, Index, IndexBuilder, NO_PARENT};
use std::ffi::OsStr;
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

/// Without Full Disk Access, macOS stops the first read of Desktop, Documents, Downloads, a
/// cloud drive, other apps' data and a few media folders for a pop-up, so `Limited` leaves
/// them out unread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Full,
    Limited,
}

/// The guarded folders, by the folder in your home that holds them.
const GUARDED: &[(&str, &[&str])] = &[
    ("", &["Desktop", "Documents", "Downloads"]),
    // iCloud Drive, cloud drives such as Dropbox, and other apps' data, which macOS 14 guards.
    // Only your ignore list could bring the rest back: Calendar and Contacts each ask too,
    // as a Terminal running `find ~` shows.
    (
        "Library",
        &[
            "Mobile Documents",
            "CloudStorage",
            "Containers",
            "Group Containers",
            "Calendars",
        ],
    ),
    ("Library/Application Support", &["AddressBook"]),
    // Found by trying on macOS 26: the Music and TV apps' folders ask for the media library,
    // and the Photos and Photo Booth libraries for Photos. A library carried over from iTunes
    // is assumed to ask the same; not tried.
    ("Music", &["Music", "iTunes"]),
    ("Movies", &["TV"]),
    (
        "Pictures",
        &["Photos Library.photoslibrary", "Photo Booth Library"],
    ),
];

type Guarded = Vec<(PathBuf, &'static [&'static str])>;

/// What decides which folders the scan reads, shared with the check on changed folders.
#[derive(Clone, Debug)]
pub(crate) struct Rules {
    clutter: Clutter,
    access: Access,
    /// Empty with Full Disk Access.
    guarded: Guarded,
    ignore: Ignore,
    home: PathBuf,
    library: PathBuf,
}

/// A folder the scan opens and does not seal.
#[derive(Clone, Copy)]
pub(crate) struct Inside {
    /// Read only on the way to a folder you brought back; nothing else inside is kept.
    pub(crate) passage: bool,
    /// ~/Library, where only cloud drives are kept.
    in_library: bool,
}

/// Worked out before a folder is read.
#[derive(Clone, Copy)]
pub(crate) struct Opening {
    skip: bool,
    kept: bool,
    may_seal: bool,
    package: bool,
}

impl Rules {
    pub(crate) fn new(home: &Path, clutter: Clutter, ignore: &Ignore, access: Access) -> Self {
        let guarded = match access {
            Access::Full => Vec::new(),
            Access::Limited => GUARDED
                .iter()
                .map(|&(parent, names)| (home.join(parent), names))
                .collect(),
        };
        Self {
            clutter,
            access,
            guarded,
            ignore: ignore.clone(),
            home: home.to_path_buf(),
            library: home.join("Library"),
        }
    }

    pub(crate) fn is_guarded(&self, path: &Path) -> bool {
        is_guarded(path, &self.guarded)
    }

    /// `None` when the folder is listed in its parent but never opened. What you scan is
    /// always read, whatever it is.
    pub(crate) fn opening(&self, path: &Path, top: bool, passage: bool) -> Option<Opening> {
        let skip = self.clutter == Clutter::Skip && !top;
        let name = path.file_name().unwrap_or_default();
        // A Photos library may be guarded under any name or folder, and a guard beats your list.
        if self.access == Access::Limited
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("photoslibrary"))
        {
            return None;
        }
        let kept = self.ignore.keeps(path, name);
        let may_seal = skip && !passage && !kept;
        let package = may_seal && is_package(path);
        if package && !self.ignore.leads_to_kept(path) {
            return None;
        }
        Some(Opening {
            skip,
            kept,
            may_seal,
            package,
        })
    }

    /// `None` when the folder is sealed. `has` tells whether the folder holds a name.
    pub(crate) fn inside(
        &self,
        path: &Path,
        opening: Opening,
        passage: bool,
        has: impl Fn(&str) -> bool,
    ) -> Option<Inside> {
        let sealed = opening.package || (opening.may_seal && is_sealed(path, has));
        if sealed && !self.ignore.leads_to_kept(path) {
            return None;
        }
        // Only cloud drives are kept from ~/Library: iCloud Drive, Dropbox, Google Drive, OneDrive.
        let in_library = opening.skip && !opening.kept && path == self.library;
        Some(Inside {
            passage: passage || sealed || in_library,
            in_library,
        })
    }

    /// The guarded folders inside `folder`. Only the few folders in GUARDED hold one, so no
    /// other folder compares names.
    pub(crate) fn guarded_in(&self, folder: &Path) -> &[&str] {
        self.guarded
            .iter()
            .find(|(parent, _)| *parent == folder)
            .map_or(&[][..], |&(_, names)| names)
    }

    /// Whether `name` is kept from `parent`: `None` when it is dropped, else whether it is a
    /// passage. `child` is its path when it is a folder, and `guarded` comes from `guarded_in`.
    pub(crate) fn keeps(
        &self,
        parent: &Path,
        inside: Inside,
        guarded: &[&str],
        name: &OsStr,
        child: Option<&Path>,
        has: impl Fn(&str) -> bool,
    ) -> Option<bool> {
        match child {
            None if inside.passage => None,
            None => Some(false),
            Some(_) if guarded.iter().any(|guarded| name == *guarded) => None,
            Some(child) => self.keeps_folder(parent, inside, child, name, has),
        }
    }

    /// Whether folder `child`, named `name`, is kept from `parent`: `None` when it is
    /// dropped, else whether it is a passage. `beside` tells whether `parent` holds a name.
    pub(crate) fn keeps_folder(
        &self,
        parent: &Path,
        inside: Inside,
        child: &Path,
        name: &OsStr,
        beside: impl Fn(&str) -> bool,
    ) -> Option<bool> {
        if self.clutter == Clutter::Keep {
            return Some(false);
        }
        let cloud = inside.in_library && (name == "Mobile Documents" || name == "CloudStorage");
        let wanted = self.ignore.keeps(child, name) || (cloud && !self.ignore.skips(child, name));
        let dropped = inside.passage
            || is_clutter(parent, name, &self.home)
            || is_tool_download(name, beside)
            || self.ignore.skips(child, name);
        if !wanted && dropped {
            // A skipped folder on the way to one you brought back is walked through, keeping
            // only the way there.
            return self.ignore.leads_to_kept(child).then_some(true);
        }
        Some(false)
    }
}

struct Walk {
    shared: Mutex<Shared>,
    rules: Rules,
    root: EntryId,
}

struct Shared {
    builder: IndexBuilder,
    unreadable_folders: usize,
}

/// Symbolic links are listed but never followed. Your ignore list counts only when clutter
/// is skipped. `home` is a real path, as `home()` gives it.
pub fn scan(
    root: &Path,
    home: &Path,
    clutter: Clutter,
    ignore: &Ignore,
    access: Access,
) -> io::Result<Scan> {
    // The real path, as macOS reports file changes: /tmp becomes /private/tmp, no "..".
    let root = fs::canonicalize(root)?;
    let rules = Rules::new(home, clutter, ignore, access);
    if rules.is_guarded(&root) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // A root you may not open, or a file, is an error with the system's own number.
    fs::read_dir(&root)?;

    let mut builder = IndexBuilder::new();
    let root_id = builder.push(&root.to_string_lossy(), NO_PARENT, true);
    let walk = Walk {
        shared: Mutex::new(Shared {
            builder,
            unreadable_folders: 0,
        }),
        rules,
        root: root_id,
    };
    rayon::scope(|scope| read_folder(scope, root, root_id, false, &walk));

    let shared = walk.shared.into_inner().unwrap();
    Ok(Scan {
        index: shared.builder.finish(),
        unreadable_folders: shared.unreadable_folders,
    })
}

/// The real home folder, the way the scan reports paths, or empty when there is none.
pub fn home() -> PathBuf {
    std::env::home_dir()
        .and_then(|home| fs::canonicalize(home).ok())
        .unwrap_or_default()
}

/// A guarded folder, or one inside it. `path` is real, so ~/documents is already ~/Documents.
fn is_guarded(path: &Path, guarded: &Guarded) -> bool {
    guarded
        .iter()
        .any(|(parent, names)| names.iter().any(|name| path.starts_with(parent.join(name))))
}

fn is_clutter(folder: &Path, name: &OsStr, home: &Path) -> bool {
    name.as_encoded_bytes().starts_with(b".")
        || name == "node_modules"
        || name == "__pycache__"
        || (name == "mod" && folder.strip_prefix(home) == Ok(Path::new("go/pkg")))
}

/// Downloads a tool keeps beside its project, known by the project file next to them.
/// A folder that happens to be called "vendor" is kept.
fn is_tool_download(name: &OsStr, beside: impl Fn(&str) -> bool) -> bool {
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

fn is_package(path: &Path) -> bool {
    let extension = path.extension().and_then(OsStr::to_str);
    extension.is_some_and(|extension| {
        PACKAGES
            .iter()
            .any(|package| package.eq_ignore_ascii_case(extension))
    })
}

/// Apps, documents saved as folders, and folders a tool labelled as its cache, are listed in
/// their parent with nothing inside them, like packages.
fn is_sealed(path: &Path, has: impl Fn(&str) -> bool) -> bool {
    let extension = path.extension().and_then(OsStr::to_str);
    match extension.map(str::to_ascii_lowercase).as_deref() {
        // Ordinary folders get these names too, like a project called satudental.app.
        Some("app" | "appex" | "bundle" | "framework" | "plugin") => {
            has("Contents") || has("Versions") || has("Info.plist")
        }
        Some("key" | "numbers" | "pages") => has("Index.zip") || has("Index"),
        Some("band") => has("projectData") || has("Alternatives"),
        Some("playground") => has("contents.xcplayground"),
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
    let rules = &walk.rules;
    let Some(opening) = rules.opening(&path, folder == walk.root, passage) else {
        return;
    };
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
    let has = |file: &str| listing.iter().any(|(name, _)| name == file);
    let Some(inside) = rules.inside(&path, opening, passage, has) else {
        return;
    };
    let guarded = rules.guarded_in(&path);

    // Names are gathered first so the shared lock is taken once per folder, not once per file.
    let mut names = String::new();
    let mut children: Vec<(usize, bool)> = Vec::new();
    let mut subfolders = Vec::new();
    for (name, is_dir) in &listing {
        let is_dir = *is_dir;
        let child_path = is_dir.then(|| path.join(name));
        let Some(child_passage) =
            rules.keeps(&path, inside, guarded, name, child_path.as_deref(), has)
        else {
            continue;
        };
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
        super::scan(root, &home(), clutter, &Ignore::default(), Access::Full)
    }

    fn scan_from(root: &Path, clutter: Clutter, home: PathBuf) -> io::Result<Scan> {
        super::scan(root, &home, clutter, &Ignore::default(), Access::Full)
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
        let (ignore, problems) = Ignore::parse(rules, root, Access::Full);
        assert!(problems.is_empty());

        let index = super::scan(root, Path::new(""), Clutter::Skip, &ignore, Access::Full).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn never_opens_a_package() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("Photos Library 2.photoslibrary");
        fs::create_dir(&photos).unwrap();
        fs::set_permissions(&photos, fs::Permissions::from_mode(0o000)).unwrap();

        let scan = scan(dir.path(), Clutter::Skip);
        fs::set_permissions(&photos, fs::Permissions::from_mode(0o755)).unwrap();

        let scan = scan.unwrap();
        assert_eq!(scan.index.len(), 2);
        assert_eq!(scan.unreadable_folders, 0);
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
            "satudental.app/src/main.ts",
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
            "satudental.app",
            "satudental.app/src",
            "satudental.app/src/main.ts",
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
            let (ignore, _) = Ignore::parse(rules, &home, Access::Full);
            let index = super::scan(root, &home, Clutter::Skip, &ignore, Access::Full);
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
    fn without_full_disk_access_the_guarded_folders_stay_unread() {
        let dir = tempfile::tempdir().unwrap();
        let home = &fs::canonicalize(dir.path()).unwrap();
        for file in [
            "Desktop/a.png",
            "Documents/b.pdf",
            "Downloads/c.zip",
            "Library/Mobile Documents/com~apple~CloudDocs/d.pdf",
            "Library/CloudStorage/Dropbox/e.md",
            "Library/Containers/com.x/Data/m.txt",
            "Library/Group Containers/group.x/n.txt",
            "Library/Calendars/p.ics",
            "Library/Application Support/AddressBook/q.abcddb",
            "Music/Music/Media.localized/h.m4a",
            "Music/iTunes/iTunes Media/r.m4a",
            "Movies/TV/Media.localized/k.mp4",
            "Pictures/Photos Library.photoslibrary/originals/l.heic",
            "Pictures/Family.photoslibrary/originals/o.heic",
            "Pictures/Photo Booth Library/Pictures/i.jpg",
            "Pictures/j.png",
            "Movies/f.mov",
            "work/Documents/g.txt",
        ] {
            fs::create_dir_all(home.join(file).parent().unwrap()).unwrap();
            fs::write(home.join(file), "").unwrap();
        }
        // Even a folder you brought back is not read inside a guarded one.
        let rules = "!~/Documents\n!~/Downloads/inside\n!~/Library\n!~/Library/Containers\n\
                     !Group Containers\n!~/Pictures/Family.photoslibrary";
        let (ignore, _) = Ignore::parse(rules, home, Access::Full);
        let paths = |access: Access, clutter: Clutter| {
            let scan = super::scan(home, home, clutter, &ignore, access).unwrap();
            assert_eq!(scan.unreadable_folders, 0);
            paths_below(home, &scan.index)
        };
        let guarded = [
            "Desktop",
            "Documents/b.pdf",
            "Downloads",
            "Library/Mobile Documents",
            "Library/CloudStorage/Dropbox/e.md",
            "Library/Containers/com.x/Data/m.txt",
            "Library/Group Containers/group.x/n.txt",
            "Library/Calendars/p.ics",
            "Library/Application Support/AddressBook/q.abcddb",
            "Music/Music",
            "Music/iTunes",
            "Movies/TV",
            "Pictures/Photos Library.photoslibrary",
            "Pictures/Family.photoslibrary/originals/o.heic",
            "Pictures/Photo Booth Library/Pictures/i.jpg",
        ];

        for clutter in [Clutter::Skip, Clutter::Keep] {
            let full = paths(Access::Full, clutter);
            let limited = paths(Access::Limited, clutter);
            for path in guarded {
                assert!(full.contains(Path::new(path)), "{path} with {clutter:?}");
                assert!(
                    !limited.contains(Path::new(path)),
                    "{path} with {clutter:?}"
                );
            }
            assert!(!limited.contains(Path::new("Documents")));
            // Only those folders, not every folder sharing their names.
            for path in [
                "Movies/f.mov",
                "Music",
                "Pictures/j.png",
                "work/Documents/g.txt",
            ] {
                assert!(limited.contains(Path::new(path)), "{path} with {clutter:?}");
            }
        }

        for root in [
            "Documents",
            "documents",
            "Library/CloudStorage/Dropbox",
            "Music/Music",
        ] {
            let limited = super::scan(
                &home.join(root),
                home,
                Clutter::Skip,
                &ignore,
                Access::Limited,
            );
            assert_eq!(
                limited.err().map(|error| error.kind()),
                Some(io::ErrorKind::PermissionDenied),
                "{root}"
            );
        }
    }

    #[test]
    fn go_downloads_are_clutter_only_in_the_home_folder() {
        let home = Path::new("/Users/ana");
        assert!(is_clutter(&home.join("go/pkg"), OsStr::new("mod"), home));
        assert!(!is_clutter(&home.join("work/pkg"), OsStr::new("mod"), home));
    }
}
