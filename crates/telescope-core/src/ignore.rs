//! Your own ignore list: folders to skip on top of the built-in clutter rules, and skipped
//! folders to bring back.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

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
    /// The rules, and a note for each line that is not one.
    pub fn parse(text: &str, home: &Path) -> (Self, Vec<String>) {
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
            // The scan reports real paths, so /tmp/build must become /private/tmp/build.
            let path = path.map(|path| fs::canonicalize(&path).unwrap_or(path));
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
        let (ignore, problems) = Ignore::parse(text, home);

        assert!(ignore.skips(Path::new("/a/Old Projects"), OsStr::new("Old Projects")));
        assert!(ignore.skips(&home.join("movies/exports"), OsStr::new("exports")));
        assert!(!ignore.skips(Path::new("/a/Exports"), OsStr::new("Exports")));
        assert!(ignore.keeps(&home.join(".config/nvim"), OsStr::new("nvim")));
        assert!(ignore.leads_to_kept(&home.join(".config")));
        assert!(!ignore.leads_to_kept(&home.join(".conf")));
        assert!(!ignore.leads_to_kept(&home.join(".config/nvim")));
        assert!(ignore.keeps(Path::new("/a/.github"), OsStr::new(".github")));
        assert_eq!(problems.len(), 3);
        assert!(problems[0].starts_with("line 7:"));
    }
}
