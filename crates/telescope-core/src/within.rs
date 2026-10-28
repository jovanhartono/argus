//! A search inside one folder: which entries sit below it.

use crate::index::{EntryId, Index, NO_PARENT, NameId};
use std::ops::Range;
use std::path::{Component, Path};

/// The entries inside one folder of one index, worked out once and kept for every search typed
/// while you stay in that folder. Only good for the index it was made from.
#[derive(Debug)]
pub struct Within {
    folder: EntryId,
    /// One bit for each entry from `folder` on: set for `folder` and every folder inside it.
    inside: Vec<u64>,
    /// Every entry inside the folder sits in this range, so a search reads only that part.
    pub(crate) entries: Range<usize>,
    /// One bit per distinct name: set for the names of the entries inside and of the folders
    /// on the way down to them, the only names a result's path can hold.
    pub(crate) names: Vec<u64>,
}

impl Within {
    /// The folder at `path`, a full path as `Index::path` gives it. `None` for a file, or a
    /// path the index doesn't hold.
    pub fn folder(index: &Index, path: &Path) -> Option<Self> {
        index.folder_at(path).map(|folder| Self::new(index, folder))
    }

    pub(crate) fn new(index: &Index, folder: EntryId) -> Self {
        let mut within = Self {
            folder,
            inside: vec![0; (index.len() - folder as usize).div_ceil(64)],
            entries: 0..0,
            names: vec![0; index.distinct_names().div_ceil(64)],
        };
        within.set(folder);
        // A folder is numbered after the folder holding it, so one pass in order finds them all.
        let after = index.folders.partition_point(|&entry| entry <= folder);
        for &entry in &index.folders[after..] {
            if within.holds(index.entry_parent[entry as usize]) {
                within.set(entry);
            }
        }
        let below = folder as usize + 1;
        let parents = &index.entry_parent[below..];
        if let Some(first) = parents.iter().position(|&parent| within.holds(parent)) {
            let last = parents.iter().rposition(|&parent| within.holds(parent));
            within.entries = below + first..below + last.unwrap_or(first) + 1;
        }
        for entry in within.entries.clone() {
            if within.holds(index.entry_parent[entry]) {
                within.add_name(index.entry_name[entry]);
            }
        }
        let mut above = folder;
        while above != NO_PARENT {
            within.add_name(index.entry_name[above as usize]);
            above = index.entry_parent[above as usize];
        }
        within
    }

    fn add_name(&mut self, name: NameId) {
        self.names[name as usize / 64] |= 1 << (name % 64);
    }

    /// Whether an entry whose folder is `parent` sits inside.
    #[inline]
    pub(crate) fn holds(&self, parent: EntryId) -> bool {
        let Some(at) = parent.checked_sub(self.folder) else {
            return false;
        };
        let at = at as usize;
        self.inside
            .get(at / 64)
            .is_some_and(|bits| bits >> (at % 64) & 1 == 1)
    }

    fn set(&mut self, entry: EntryId) {
        let at = (entry - self.folder) as usize;
        self.inside[at / 64] |= 1 << (at % 64);
    }
}

impl Index {
    /// The folder at `path`, a full path as `Index::path` gives it.
    pub(crate) fn folder_at(&self, path: &Path) -> Option<EntryId> {
        let (root, below) = self.roots.iter().find_map(|&root| {
            let below = path.strip_prefix(self.name(root)).ok()?;
            Some((root, below))
        })?;
        let names: Option<Vec<&str>> = below
            .components()
            .map(|part| match part {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect();
        self.find_folder(root, names?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexBuilder;
    use crate::{Access, Apps, Clutter, Ignore, Searcher, Settings};
    use std::fs;
    use std::path::PathBuf;

    fn inside(index: &Index, folder: EntryId) -> Vec<EntryId> {
        let within = Within::new(index, folder);
        index
            .ids()
            .filter(|&entry| within.holds(index.entry_parent[entry as usize]))
            .collect()
    }

    /// What lies inside `folder`, found by climbing from every entry.
    fn climbed(index: &Index, folder: EntryId) -> Vec<EntryId> {
        let climbs = |entry: EntryId| {
            let mut at = index.entry_parent[entry as usize];
            while at != NO_PARENT && at != folder {
                at = index.entry_parent[at as usize];
            }
            at == folder
        };
        index.ids().filter(|&entry| climbs(entry)).collect()
    }

    fn sample() -> Index {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home/me", NO_PARENT, true);
        let work = builder.push("work", root, true);
        let notes = builder.push("notes", root, true);
        builder.push("plan.md", work, false);
        let deep = builder.push("deep", notes, true);
        // Children of `work` and `notes` apart, as in made-up names.
        builder.push("a.txt", deep, false);
        builder.push("b", work, true);
        builder.push("notes", work, true);
        let mut index = builder.finish();
        index.children_apart = true;
        index
    }

    #[test]
    fn finds_a_folder_by_its_full_path() {
        let index = sample();
        let at = |path: &str| index.folder_at(Path::new(path));
        assert_eq!(at("/home/me"), Some(0));
        assert_eq!(at("/home/me/"), Some(0));
        assert_eq!(at("/home/me/work"), Some(1));
        assert_eq!(at("/home/me/notes"), Some(2));
        assert_eq!(at("/home/me/notes/deep"), Some(4));
        assert_eq!(at("/home/me/work/b"), Some(6));
        assert_eq!(at("/home/me/work/notes"), Some(7));
        assert_eq!(at("/home/me/work/plan.md"), None, "a file");
        assert_eq!(at("/home/me/work/missing"), None);
        assert_eq!(at("/home/me/work/../notes"), None);
        assert_eq!(at("/home/me/./work"), Some(1), "Path drops a lone dot");
        assert_eq!(at("/home"), None);
        assert_eq!(at("/home/meme"), None);
        assert_eq!(at("work"), None);
        assert_eq!(at(""), None);
    }

    #[test]
    fn finds_a_folder_on_a_volume_and_holds_only_what_is_on_it() {
        let mut builder = IndexBuilder::new();
        let home = builder.push("/home/me", NO_PARENT, true);
        let work = builder.push("work", home, true);
        builder.push("plan.md", work, false);
        let volume = builder.push("/Volumes/Photos", NO_PARENT, true);
        let shoot = builder.push("work", volume, true);
        builder.push("beach.jpg", shoot, false);
        let index = builder.finish();
        let at = |path: &str| index.folder_at(Path::new(path));
        assert_eq!(at("/Volumes/Photos"), Some(volume));
        assert_eq!(at("/Volumes/Photos/work"), Some(shoot));
        assert_eq!(at("/home/me/work"), Some(work));
        assert_eq!(at("/Volumes/Photos/missing"), None);
        assert_eq!(at("/Volumes"), None);
        for folder in index.folders.clone() {
            assert_eq!(inside(&index, folder), climbed(&index, folder), "{folder}");
        }
        assert_eq!(inside(&index, home), [work, work + 1]);
    }

    #[test]
    fn holds_what_climbing_from_each_entry_finds() {
        let index = sample();
        for folder in index.folders.clone() {
            assert_eq!(inside(&index, folder), climbed(&index, folder), "{folder}");
        }
        assert_eq!(
            inside(&index, 3),
            Vec::<EntryId>::new(),
            "a file holds nothing"
        );
        assert_eq!(Within::new(&index, 3).entries, 0..0);
        assert_eq!(Within::new(&index, 2).entries, 4..6);
        assert_eq!(Within::new(&index, 0).entries, 1..8);
        let within = Within::new(&index, 4);
        assert!(!within.holds(NO_PARENT));
        assert!(!within.holds(0));
        assert!(!within.holds(3));
    }

    #[test]
    fn the_last_folder_in_the_list_holds_its_own_children() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/r", NO_PARENT, true);
        let mut folder = root;
        for depth in 0..70 {
            folder = builder.push(&format!("d{depth}"), folder, true);
        }
        builder.push("leaf.txt", folder, false);
        let index = builder.finish();
        assert_eq!(inside(&index, folder), [71]);
        assert_eq!(inside(&index, 1).len(), 70);
        assert_eq!(Within::new(&index, 1).entries, 2..72);
    }

    /// An update leaves removed entries in place and adds new ones at the end, so a folder's
    /// children sit apart; the search inside it must still find exactly what lies inside.
    #[test]
    fn holds_the_right_entries_after_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for a in 0..4 {
            for b in 0..4 {
                let folder = root.join(format!("a{a}/b{b}"));
                fs::create_dir_all(&folder).unwrap();
                // Enough that the update leaves removed entries in place rather than compacting.
                for f in 0..40 {
                    fs::write(folder.join(format!("plan {f}.md")), "").unwrap();
                }
            }
        }
        fs::create_dir(root.join("a1/small")).unwrap();
        fs::write(root.join("a1/small/plan s.md"), "").unwrap();
        let settings = Settings::new(&root, &root, Clutter::Keep, Ignore::default(), Access::Full);
        let mut index = settings.scan().unwrap().index;
        fs::remove_dir_all(root.join("a1/small")).unwrap();
        fs::create_dir(root.join("a1/new")).unwrap();
        fs::write(root.join("a1/new/plan x.md"), "").unwrap();
        fs::write(root.join("a1/plan y.md"), "").unwrap();
        fs::remove_file(root.join("a2/b0/plan 3.md")).unwrap();
        let changed: Vec<PathBuf> = ["a1", "a2/b0"].iter().map(|p| root.join(p)).collect();
        let update = settings.update(&mut index, &changed);
        assert!(
            update.removed > 0 && update.added > 0 && !update.compacted,
            "{update:?}"
        );

        let searcher = Searcher::with_threads(2);
        for folder in ["", "a1", "a1/new", "a2/b0", "a3"] {
            let scope = root.join(folder);
            let within = Within::folder(&index, &scope).unwrap();
            let expected: Vec<EntryId> = climbed(&index, within.folder)
                .into_iter()
                .filter(|&entry| !index.is_removed(entry))
                .collect();
            let held: Vec<EntryId> = index
                .ids()
                .filter(|&entry| within.holds(index.entry_parent[entry as usize]))
                .collect();
            assert_eq!(held, expected, "{folder}");
            let apps = Apps::default();
            let ticket = searcher.ticket();
            let hits = searcher.search_in(&index, &apps, Some(&within), "plan", ticket, 1_000);
            let mut found: Vec<PathBuf> = hits
                .unwrap()
                .iter()
                .map(|hit| index.path(hit.entry().unwrap()))
                .collect();
            let mut on_disk: Vec<PathBuf> = expected
                .iter()
                .map(|&entry| index.path(entry))
                .filter(|path| path.extension().is_some())
                .collect();
            found.sort();
            on_disk.sort();
            assert_eq!(found, on_disk, "{folder}");
            assert!(
                found
                    .iter()
                    .all(|path| path.starts_with(&scope) && path.exists())
            );
        }
        assert!(
            Within::folder(&index, &root.join("a1/small")).is_none(),
            "removed"
        );
        assert!(index.folder_at(&root.join("a1/b1")).is_some());
    }
}
