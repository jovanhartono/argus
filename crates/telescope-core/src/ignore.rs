//! Your own ignore list: folders to skip on top of the built-in clutter rules, and skipped
//! folders to bring back.

use crate::Access;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// Where your ignore list lives, inside your home folder.
const IGNORE_FILE: &str = ".config/telescope/ignore";

/// One folder per line. A plain name skips every folder with that name, a path starting
/// with `~/` or `/` skips that one folder, and `!` in front brings back a folder the rules
/// skip. A `!` path works even deep inside a skipped folder; a `!` name only where its
/// parent is read. `#` starts a comment. A to Z match in either case, as on a Mac's disk.
#[derive(Clone, Debug, Default)]
pub struct Ignore {
    skip_names: Vec<OsString>,
    skip_paths: Vec<PathBuf>,
    keep_names: Vec<OsString>,
    keep_paths: Vec<PathBuf>,
}

impl Ignore {
    /// Your list from `~/.config/telescope/ignore`, and a note for each problem with it.
    /// A missing file is the normal case and means no rules of your own.
    pub fn load(home: &Path, access: Access) -> (Self, Vec<String>) {
        if home.as_os_str().is_empty() {
            return Default::default();
        }
        // Dotfiles kept in Documents or iCloud Drive and linked in would bring up a pop-up.
        if access == Access::Limited && is_linked(home) {
            let problem =
                format!("~/{IGNORE_FILE} is a link, read once Telescope has Full Disk Access");
            return (Self::default(), vec![problem]);
        }
        let file = home.join(IGNORE_FILE);
        match fs::read_to_string(&file) {
            Ok(text) => {
                let (ignore, problems) = Self::parse(&text, home, access);
                let problems = problems
                    .into_iter()
                    .map(|problem| format!("~/{IGNORE_FILE} {problem}"))
                    .collect();
                (ignore, problems)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
            Err(error) => {
                let problem = format!("cannot read {}: {error}", file.display());
                (Self::default(), vec![problem])
            }
        }
    }

    /// The rules, and a note for each line that is not one.
    pub fn parse(text: &str, home: &Path, access: Access) -> (Self, Vec<String>) {
        let mut ignore = Self::default();
        let mut problems = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keep, rule) = match line.strip_prefix('!') {
                Some(rule) => (true, rule.trim()),
                None => (false, line),
            };
            let rule = rule.trim_end_matches('/');
            // Rebuilt from its parts so a rule written with / matches Windows paths too.
            let path = match rule.strip_prefix("~/") {
                Some(inside_home) => {
                    Some(home.join(Path::new(inside_home).components().collect::<PathBuf>()))
                }
                None if rule.starts_with('/') => Some(Path::new(rule).components().collect()),
                None => None,
            };
            let path = path.map(|path| real(path, access));
            match (path, keep) {
                (Some(path), false) => ignore.skip_paths.push(path),
                (Some(path), true) => ignore.keep_paths.push(path),
                (None, _) if rule.is_empty() || rule == "~" || rule.contains('/') => {
                    problems.push(format!(
                        "line {}: \"{line}\" is not a folder name, or a path starting with ~/ or /",
                        number + 1
                    ))
                }
                (None, false) => ignore.skip_names.push(rule.into()),
                (None, true) => ignore.keep_names.push(rule.into()),
            }
        }
        (ignore, problems)
    }

    pub(crate) fn skips(&self, path: &Path, name: &OsStr) -> bool {
        self.skip_names.iter().any(|skip| same(skip, name))
            || self
                .skip_paths
                .iter()
                .any(|skip| same(skip.as_os_str(), path.as_os_str()))
    }

    pub(crate) fn keeps(&self, path: &Path, name: &OsStr) -> bool {
        self.keep_names.iter().any(|keep| same(keep, name))
            || self
                .keep_paths
                .iter()
                .any(|keep| same(keep.as_os_str(), path.as_os_str()))
    }

    /// Whether a folder you brought back sits somewhere inside `folder`.
    pub(crate) fn leads_to_kept(&self, folder: &Path) -> bool {
        let folder = folder.as_os_str().as_encoded_bytes();
        self.keep_paths.iter().any(|keep| {
            let keep = keep.as_os_str().as_encoded_bytes();
            keep.len() > folder.len()
                && keep[..folder.len()].eq_ignore_ascii_case(folder)
                && keep[folder.len()] == MAIN_SEPARATOR as u8
        })
    }
}

/// The scan reports real paths, so /tmp/build must become /private/tmp/build and a rule
/// through a linked folder must name where it leads. Without Full Disk Access only the top
/// folder is looked up: looking inside Documents or a USB disk can bring up a pop-up.
fn real(path: PathBuf, access: Access) -> PathBuf {
    if access == Access::Full {
        return fs::canonicalize(&path).unwrap_or(path);
    }
    let mut parts = path.components();
    let top: PathBuf = parts.by_ref().take(2).collect();
    let mut real = fs::canonicalize(&top).unwrap_or(top);
    real.extend(parts);
    real
}

/// Whether the list, or a folder on the way to it, is a symbolic link.
fn is_linked(home: &Path) -> bool {
    let mut path = home.to_path_buf();
    Path::new(IGNORE_FILE).components().any(|part| {
        path.push(part);
        fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink())
    })
}

fn same(a: &OsStr, b: &OsStr) -> bool {
    a.as_encoded_bytes()
        .eq_ignore_ascii_case(b.as_encoded_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_names_paths_and_comebacks() {
        let home = Path::new("/Users/ana");
        let text = "# mine\n\nold projects\n~/Movies/Exports/\n!~/.config/nvim\n! .github\n\
                    Documents/x\n!\n~/\n";
        let (ignore, problems) = Ignore::parse(text, home, Access::Full);

        assert!(ignore.skips(Path::new("/a/Old Projects"), OsStr::new("Old Projects")));
        let nvim = home.join(".config").join("nvim");
        let exports = home.join("movies").join("exports");
        assert!(ignore.skips(&exports, OsStr::new("exports")));
        assert!(!ignore.skips(Path::new("/a/Exports"), OsStr::new("Exports")));
        assert!(ignore.keeps(&nvim, OsStr::new("nvim")));
        assert!(ignore.leads_to_kept(&home.join(".config")));
        assert!(!ignore.leads_to_kept(&home.join(".conf")));
        assert!(!ignore.leads_to_kept(&nvim));
        assert!(ignore.keeps(Path::new("/a/.github"), OsStr::new(".github")));
        assert_eq!(problems.len(), 3);
        assert!(problems[0].starts_with("line 7:"));
    }

    #[test]
    fn loads_the_list_from_the_home_folder() {
        let home = tempfile::tempdir().unwrap();
        let home = &fs::canonicalize(home.path()).unwrap();
        let (ignore, problems) = Ignore::load(home, Access::Full);
        assert!(ignore.skip_names.is_empty() && problems.is_empty());

        let file = home.join(IGNORE_FILE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "Old Projects\nwork/old\n").unwrap();
        let (ignore, problems) = Ignore::load(home, Access::Full);
        assert!(ignore.skips(&home.join("Old Projects"), OsStr::new("Old Projects")));
        assert_eq!(
            problems,
            [format!(
                "~/{IGNORE_FILE} line 2: \"work/old\" is not a folder name, or a path starting with ~/ or /"
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_list_waits_for_full_disk_access() {
        let dotfiles = tempfile::tempdir().unwrap();
        fs::write(dotfiles.path().join("ignore"), "Old Projects\n").unwrap();
        let home = tempfile::tempdir().unwrap();
        let home = &fs::canonicalize(home.path()).unwrap();
        fs::create_dir(home.join(".config")).unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), home.join(".config/telescope")).unwrap();

        let (ignore, problems) = Ignore::load(home, Access::Limited);
        assert!(ignore.skip_names.is_empty());
        assert_eq!(problems.len(), 1);
        let (ignore, problems) = Ignore::load(home, Access::Full);
        assert!(ignore.skips(&home.join("Old Projects"), OsStr::new("Old Projects")));
        assert!(problems.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_path_rule_is_made_real_from_its_top_folder() {
        let (ignore, _) = Ignore::parse("/tmp/build", Path::new("/Users/ana"), Access::Limited);
        assert!(ignore.skips(Path::new("/private/tmp/build"), OsStr::new("build")));
    }

    #[cfg(unix)]
    #[test]
    fn with_full_disk_access_a_rule_follows_linked_folders() {
        let home = tempfile::tempdir().unwrap();
        let home = &fs::canonicalize(home.path()).unwrap();
        fs::create_dir_all(home.join("work/projects/old")).unwrap();
        std::os::unix::fs::symlink(home.join("work/projects"), home.join("Projects")).unwrap();
        let old = home.join("work/projects/old");

        let (ignore, _) = Ignore::parse("~/Projects/old", home, Access::Full);
        assert!(ignore.skips(&old, OsStr::new("old")));
        let (ignore, _) = Ignore::parse("~/Projects/old", home, Access::Limited);
        assert!(!ignore.skips(&old, OsStr::new("old")));
    }
}
