//! Apps, found by reading the folders macOS installs them in.

use std::fs;
use std::path::{Path, PathBuf};

pub type AppId = u32;

/// Every app bundle in the app folders, by the name Finder shows: Safari for Safari.app.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Apps {
    pub(crate) names: Vec<Box<str>>,
    /// The folder each app sits in, by its place in `folders`.
    folder_of: Vec<u32>,
    folders: Vec<PathBuf>,
}

impl Apps {
    /// The apps in /Applications, /System/Applications and ~/Applications, and in the plain
    /// folders inside them, like Utilities, an Adobe app's own folder or Chrome's web apps.
    /// Empty on other systems.
    pub fn scan(home: &Path) -> Self {
        if !cfg!(target_os = "macos") {
            return Self::default();
        }
        Self::scan_folders(top_folders(home))
    }

    /// Reads each folder and the plain folders directly inside it, never inside an app. A
    /// missing or unreadable folder adds nothing.
    pub fn scan_folders(top: Vec<PathBuf>) -> Self {
        let mut apps = Self::default();
        // Each app reached through a link, by its place in `names`, and where the link leads.
        let mut links: Vec<(usize, PathBuf)> = Vec::new();
        let mut queue: Vec<(PathBuf, bool)> =
            top.into_iter().map(|folder| (folder, true)).collect();
        while let Some((folder, top)) = queue.pop() {
            let Ok(entries) = fs::read_dir(&folder) else {
                continue;
            };
            let id = apps.folders.len() as u32;
            let mut found = 0;
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let file_name = entry.file_name();
                // Not UTF-8 is never an app's name in practice, and hidden ones Finder hides too.
                let Some(name) = file_name.to_str().filter(|name| !name.starts_with('.')) else {
                    continue;
                };
                let Some(app) = name.strip_suffix(".app") else {
                    if top && kind.is_dir() {
                        queue.push((entry.path(), false));
                    }
                    continue;
                };
                // /Applications/Safari.app is a link into the system's sealed volume.
                if kind.is_symlink() {
                    match fs::canonicalize(entry.path()) {
                        Ok(real) if real.is_dir() => links.push((apps.names.len(), real)),
                        _ => continue,
                    }
                } else if !kind.is_dir() {
                    continue;
                }
                apps.names.push(app.into());
                apps.folder_of.push(id);
                found += 1;
            }
            if found > 0 {
                apps.folders.push(folder);
            }
        }
        apps.drop_repeated_links(&links);
        apps.names.shrink_to_fit();
        apps.folder_of.shrink_to_fit();
        apps
    }

    /// A link to an app listed already, like ~/Applications/Foo.app leading to
    /// /Applications/Foo.app, would show the same app twice.
    fn drop_repeated_links(&mut self, links: &[(usize, PathBuf)]) {
        let linked = |app: usize| links.iter().any(|&(link, _)| link == app);
        let repeated: Vec<usize> = links
            .iter()
            .enumerate()
            .filter(|&(i, (_, real))| {
                links[..i].iter().any(|(_, earlier)| earlier == real)
                    || (0..self.len()).any(|app| !linked(app) && self.path(app as AppId) == *real)
            })
            .map(|(_, &(app, _))| app)
            .collect();
        if repeated.is_empty() {
            return;
        }
        let mut app = 0;
        self.folder_of.retain(|_| {
            app += 1;
            !repeated.contains(&(app - 1))
        });
        let mut app = 0;
        self.names.retain(|_| {
            app += 1;
            !repeated.contains(&(app - 1))
        });
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The name without ".app".
    pub fn name(&self, app: AppId) -> &str {
        &self.names[app as usize]
    }

    pub fn folder(&self, app: AppId) -> &Path {
        &self.folders[self.folder_of[app as usize] as usize]
    }

    /// The bundle, the path that opens the app.
    pub fn path(&self, app: AppId) -> PathBuf {
        self.folder(app).join(format!("{}.app", self.name(app)))
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity() * size_of::<Box<str>>()
            + self.names.iter().map(|name| name.len()).sum::<usize>()
            + self.folder_of.capacity() * size_of::<u32>()
            + self.folders.capacity() * size_of::<PathBuf>()
            + self.folders.iter().map(PathBuf::capacity).sum::<usize>()
    }

    #[cfg(test)]
    pub(crate) fn from_names(folder: &Path, names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|&name| name.into()).collect(),
            folder_of: vec![0; names.len()],
            folders: vec![folder.to_path_buf()],
        }
    }
}

fn top_folders(home: &Path) -> Vec<PathBuf> {
    let mut folders = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    // `home()` is empty when there is no home folder, and a relative path would be read
    // from wherever the app was started.
    if home.is_absolute() {
        folders.push(home.join("Applications"));
    }
    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn found(apps: &Apps) -> BTreeSet<PathBuf> {
        (0..apps.len() as AppId).map(|app| apps.path(app)).collect()
    }

    fn make(root: &Path, folders: &[&str], files: &[&str]) {
        for folder in folders {
            fs::create_dir_all(root.join(folder)).unwrap();
        }
        for file in files {
            fs::write(root.join(file), "").unwrap();
        }
    }

    #[test]
    fn finds_bundles_and_those_one_plain_folder_down() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make(
            root,
            &[
                "Applications/Safari.app/Contents",
                "Applications/Visual Studio Code.app/Contents/Frameworks/Helper.app",
                "Applications/Utilities/Terminal.app",
                "Applications/Adobe Photoshop 2026/Adobe Photoshop 2026.app",
                "Applications/Adobe Photoshop 2026/Plug-ins/Deep.app",
                "Applications/Café.app",
                "Applications/Foo.app.app",
                "Applications/.Hidden.app",
                "Applications/Chrome Apps.localized/Gmail.app",
                "Applications/Upper.APP",
                "Applications/Empty",
                "System/Calculator.app",
            ],
            &[
                "Applications/notes.app",
                "Applications/Setup.dmg",
                "Applications/.app",
            ],
        );
        let apps = Apps::scan_folders(vec![
            root.join("Applications"),
            root.join("System"),
            root.join("Missing"),
        ]);
        let expected: BTreeSet<PathBuf> = [
            "Applications/Safari.app",
            "Applications/Visual Studio Code.app",
            "Applications/Utilities/Terminal.app",
            "Applications/Adobe Photoshop 2026/Adobe Photoshop 2026.app",
            "Applications/Café.app",
            "Applications/Foo.app.app",
            "Applications/Chrome Apps.localized/Gmail.app",
            "System/Calculator.app",
        ]
        .iter()
        .map(|path| root.join(path))
        .collect();
        assert_eq!(found(&apps), expected);
        let mut names: Vec<&str> = (0..apps.len() as AppId).map(|app| apps.name(app)).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Adobe Photoshop 2026",
                "Café",
                "Calculator",
                "Foo.app",
                "Gmail",
                "Safari",
                "Terminal",
                "Visual Studio Code"
            ]
        );
        // Only folders holding an app are kept.
        assert_eq!(apps.folders.len(), 5);
    }

    #[test]
    fn nothing_found_is_empty_and_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let apps = Apps::scan_folders(vec![dir.path().to_path_buf(), dir.path().join("gone")]);
        assert!(apps.is_empty());
        assert_eq!(apps.heap_bytes(), 0);
        assert!(Apps::scan_folders(Vec::new()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_link_to_an_app_but_not_a_broken_one_or_into_a_linked_folder() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make(
            root,
            &["sealed/Safari.app/Contents", "Apps", "elsewhere/Far.app"],
            &[],
        );
        symlink(root.join("sealed/Safari.app"), root.join("Apps/Safari.app")).unwrap();
        symlink(root.join("gone.app"), root.join("Apps/Broken.app")).unwrap();
        symlink(root.join("elsewhere"), root.join("Apps/Elsewhere")).unwrap();
        let apps = Apps::scan_folders(vec![root.join("Apps")]);
        assert_eq!(found(&apps), BTreeSet::from([root.join("Apps/Safari.app")]));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_an_app_listed_already_is_dropped() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = &fs::canonicalize(dir.path()).unwrap();
        make(
            root,
            &["Applications/Foo.app", "Home", "sealed/Bar.app", "Mine"],
            &[],
        );
        symlink(root.join("Applications/Foo.app"), root.join("Home/Foo.app")).unwrap();
        symlink(root.join("Applications/Foo.app"), root.join("Mine/Foo.app")).unwrap();
        symlink(
            root.join("sealed/Bar.app"),
            root.join("Applications/Bar.app"),
        )
        .unwrap();
        symlink(root.join("sealed/Bar.app"), root.join("Home/Bar.app")).unwrap();
        let apps = Apps::scan_folders(vec![
            root.join("Applications"),
            root.join("Home"),
            root.join("Mine"),
        ]);
        let names: BTreeSet<&str> = (0..apps.len() as AppId).map(|app| apps.name(app)).collect();
        // Bar is reached through two links, and whichever folder is read first keeps it.
        assert_eq!(names, BTreeSet::from(["Bar", "Foo"]));
        assert_eq!(apps.len(), 2);
        assert!(found(&apps).contains(&root.join("Applications/Foo.app")));
        assert_eq!(apps.names.len(), apps.folder_of.len());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_adds_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &["Locked/Hidden.app", "Open.app"], &[]);
        let locked = dir.path().join("Locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let apps = Apps::scan_folders(vec![dir.path().to_path_buf(), locked.clone()]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found(&apps), BTreeSet::from([dir.path().join("Open.app")]));
    }

    /// The window reads the apps on every show and ranks again only when they differ.
    #[cfg(unix)]
    #[test]
    fn the_same_folders_read_twice_are_equal_until_an_app_comes_or_goes() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make(
            root,
            &[
                "Applications/Zed.app",
                "Applications/Alpha.app",
                "Applications/Utilities/Terminal.app",
                "Applications/Adobe/Photoshop.app",
                "Applications/Empty",
                "Home/Mine.app",
                "sealed/Safari.app",
            ],
            &["Applications/notes.txt"],
        );
        symlink(
            root.join("sealed/Safari.app"),
            root.join("Applications/Safari.app"),
        )
        .unwrap();
        let top = || vec![root.join("Applications"), root.join("Home")];
        let before = Apps::scan_folders(top());
        assert_eq!(before.len(), 6);
        assert_eq!(Apps::scan_folders(top()), before);

        make(root, &["Applications/Utilities/Console.app"], &[]);
        let installed = Apps::scan_folders(top());
        assert_ne!(installed, before);
        assert_eq!(Apps::scan_folders(top()), installed);
        fs::remove_dir(root.join("Applications/Utilities/Console.app")).unwrap();
        assert_eq!(Apps::scan_folders(top()), before);
        // A plain file or an emptied folder changes nothing.
        make(root, &["Applications/More"], &["Applications/Setup.dmg"]);
        assert_eq!(Apps::scan_folders(top()), before);
    }

    #[test]
    fn counts_its_memory() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &["Notes.app", "Maps.app"], &[]);
        let apps = Apps::scan_folders(vec![dir.path().to_path_buf()]);
        let least = 2 * size_of::<Box<str>>()
            + "NotesMaps".len()
            + 2 * size_of::<u32>()
            + size_of::<PathBuf>()
            + dir.path().as_os_str().len();
        assert!(apps.heap_bytes() >= least, "{}", apps.heap_bytes());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_the_app_folder_in_your_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        make(&home, &["Applications/Telescope Test Fake.app"], &[]);
        let apps = Apps::scan(&home);
        assert!(found(&apps).contains(&home.join("Applications/Telescope Test Fake.app")));
        // Every Mac has these.
        let names: Vec<&str> = (0..apps.len() as AppId).map(|app| apps.name(app)).collect();
        assert!(names.contains(&"Calculator"), "{names:?}");
        assert!(names.contains(&"Safari"), "{names:?}");
        assert!(names.contains(&"Terminal"), "{names:?}");
    }

    #[test]
    fn a_home_that_is_not_absolute_adds_no_folder() {
        let system = [
            PathBuf::from("/Applications"),
            PathBuf::from("/System/Applications"),
        ];
        assert_eq!(top_folders(Path::new("")), system);
        assert_eq!(top_folders(Path::new("rel/home")), system);
        let home = std::env::temp_dir();
        assert_eq!(top_folders(&home)[2..], [home.join("Applications")]);
        if !cfg!(target_os = "macos") {
            assert!(Apps::scan(&home).is_empty());
        }
    }
}
