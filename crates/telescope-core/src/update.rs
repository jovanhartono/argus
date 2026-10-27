//! Keeping the index current: each changed folder is read again and the lists are patched in
//! place, so a change costs a few folder reads instead of a rescan.

use crate::index::{EntryId, Index, IndexBuilder, NO_PARENT, NameId, name_fingerprint};
use crate::walk::{Kind, Listing, Rules, walk};
use rayon::prelude::*;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Every search reads the removed entries and the names added entries store again, so the
/// lists are rebuilt once those pass a 32nd of them.
const COMPACT_SHARE: usize = 32;
/// Folders read at once; their listings are held until they are compared.
const READ_CHUNK: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Update {
    pub folders_read: usize,
    pub added: usize,
    pub removed: usize,
    /// Folders read again or added that could not be read in full. A folder read again keeps
    /// the children it did not show, as a disk error or a moment without permission does not
    /// delete them.
    pub unreadable_folders: usize,
    /// The lists were rebuilt without the removed entries.
    pub compacted: bool,
}

/// Where each folder's children sit. A scan pushes a folder's children in one go, so a folder
/// has one run of them, plus one more for each update that added to it. A run may also hold
/// removed entries, so each child found is checked against its parent.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Default)]
pub(crate) struct Runs {
    /// Each folder's first run, by the folder's place in `folders`.
    first: Vec<(EntryId, EntryId)>,
    /// Later runs as (folder, start, end), by folder.
    more: Vec<(EntryId, EntryId, EntryId)>,
}

impl Runs {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.first.capacity() * size_of::<(EntryId, EntryId)>()
            + self.more.capacity() * size_of::<(EntryId, EntryId, EntryId)>()
    }

    fn add(&mut self, folders: &[EntryId], (folder, start, end): (EntryId, EntryId, EntryId)) {
        let Ok(slot) = folders.binary_search(&folder) else {
            return;
        };
        let first = &mut self.first[slot];
        if first.0 == first.1 {
            *first = (start, end);
        } else if first.1 == start {
            first.1 = end;
        } else {
            self.more.push((folder, start, end));
        }
    }
}

impl Index {
    /// Reads each changed folder again and brings its part of the index up to date: removed
    /// children leave with everything inside them, new ones are added with everything the
    /// rules keep inside them, and a move is a removal plus an addition. A path that is not
    /// an indexed folder updates the deepest indexed folder on its way, so a file's own path,
    /// a new folder's or a deleted one's all work.
    pub(crate) fn update<P: AsRef<Path>>(
        &mut self,
        root: &Path,
        rules: &Rules,
        changed: &[P],
    ) -> Update {
        let mut update = Update::default();
        if self.folders.first() != Some(&0) {
            return update;
        }
        if self.runs.is_none() {
            self.runs = Some(Runs::default());
            self.add_runs(0);
        }
        let changed: Vec<&Path> = changed.iter().map(AsRef::as_ref).collect();
        let mut targets = self.changed_folders(root, &changed);
        // Parents first, so a folder its parent replaced is not read into the old one.
        targets.sort_unstable_by_key(|&(folder, _)| folder);
        targets.dedup_by_key(|&mut (folder, _)| folder);
        let mut added = Added {
            builder: IndexBuilder::new(),
            anchors: Vec::new(),
            walks: Vec::new(),
        };
        for chunk in targets.chunks(READ_CHUNK) {
            let listings: Vec<(Listing, bool)> = chunk
                .par_iter()
                .map(|(folder, at)| {
                    let passage = self.passages.binary_search(folder).is_ok();
                    let listing = rules.list(at, *folder == 0, passage);
                    // A folder deleted since it was found lost everything in it.
                    let partial = listing.unreadable
                        && fs::symlink_metadata(at).is_ok_and(|meta| meta.is_dir());
                    (listing, partial)
                })
                .collect();
            for (&(folder, _), (listing, partial)) in chunk.iter().zip(listings) {
                if folder != 0 && self.entry_parent[folder as usize] == NO_PARENT {
                    continue;
                }
                update.folders_read += 1;
                update.unreadable_folders += usize::from(partial);
                update.removed += self.apply(folder, listing, partial, &mut added);
            }
        }
        if !added.anchors.is_empty() {
            let walked = walk(added.builder, rules, NO_PARENT, added.walks);
            update.unreadable_folders += walked.unreadable_folders;
            update.added = self.splice(&walked.builder.lists(), &added.anchors);
        }

        if (self.removed + self.added) * COMPACT_SHARE > self.len() {
            self.compact();
            update.compacted = true;
        }
        update
    }

    /// For each changed path, the deepest indexed folder on it that is still a folder on disk,
    /// and that folder's path. A path outside the root has none.
    fn changed_folders(&self, root: &Path, changed: &[&Path]) -> Vec<(EntryId, PathBuf)> {
        let mut paths: Vec<(Vec<Cow<str>>, &Path)> = changed
            .iter()
            .filter_map(|path| {
                let below = path.strip_prefix(root).ok()?;
                let names = below
                    .components()
                    .map_while(|part| match part {
                        Component::Normal(name) => Some(name.to_string_lossy()),
                        _ => None,
                    })
                    .collect();
                Some((names, below))
            })
            .collect();
        // Paths into one folder sit together, so its children are looked up once for them all.
        paths.sort_unstable();
        let mut found = Vec::with_capacity(paths.len());
        self.descend(0, 0, &paths, &mut found);
        found
            .into_par_iter()
            .map(|(mut folder, depth, below)| {
                let mut at = root.to_path_buf();
                at.extend(below.components().take(depth));
                // A folder deleted or turned into a file is read from its parent, which lost it.
                while folder != 0 && !fs::symlink_metadata(&at).is_ok_and(|meta| meta.is_dir()) {
                    folder = self.entry_parent[folder as usize];
                    at.pop();
                }
                (folder, at)
            })
            .collect()
    }

    /// Finds the folder for each of `paths`, which are sorted and all lead through `folder`,
    /// `depth` names below the root.
    fn descend<'p>(
        &self,
        folder: EntryId,
        depth: usize,
        paths: &[(Vec<Cow<str>>, &'p Path)],
        found: &mut Vec<(EntryId, usize, &'p Path)>,
    ) {
        let ends = paths.partition_point(|(names, _)| names.len() == depth);
        found.extend(
            paths[..ends]
                .iter()
                .map(|&(_, below)| (folder, depth, below)),
        );
        let paths = &paths[ends..];
        if paths.is_empty() {
            return;
        }
        let mut children: Vec<(&str, EntryId)> = self
            .run_children(folder)
            .filter(|&child| self.is_dir(child))
            .map(|child| (self.name(child), child))
            .collect();
        children.sort_unstable();
        for group in paths.chunk_by(|a, b| a.0[depth] == b.0[depth]) {
            let name = &*group[0].0[depth];
            match children.binary_search_by(|&(child, _)| child.cmp(name)) {
                Ok(at) => self.descend(children[at].1, depth + 1, group, found),
                Err(_) => found.extend(group.iter().map(|&(_, below)| (folder, depth, below))),
            }
        }
    }

    fn run_children(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        let runs = self
            .runs
            .as_ref()
            .expect("runs are found before any update");
        let first = self
            .folders
            .binary_search(&folder)
            .map_or((0, 0), |slot| runs.first[slot]);
        let from = runs.more.partition_point(|&(f, ..)| f < folder);
        let more = runs.more[from..]
            .iter()
            .take_while(move |&&(f, ..)| f == folder)
            .map(|&(_, start, end)| (start, end));
        std::iter::once(first)
            .chain(more)
            .flat_map(|(start, end)| start..end)
            .filter(move |&child| self.entry_parent[child as usize] == folder)
    }

    /// What the entry is, and its inode when it is a folder.
    fn kind(&self, entry: EntryId) -> (Kind, u32) {
        let Ok(slot) = self.folders.binary_search(&entry) else {
            return (Kind::File, 0);
        };
        let kind = if self.passages.binary_search(&entry).is_ok() {
            Kind::Passage
        } else {
            Kind::Folder
        };
        (kind, self.folder_inodes[slot])
    }

    /// Compares a folder's listing with its children in the index. Returns how many entries
    /// were removed; new ones go to `added`.
    fn apply(
        &mut self,
        folder: EntryId,
        mut listing: Listing,
        partial: bool,
        added: &mut Added,
    ) -> usize {
        let mut paths = std::mem::take(&mut listing.folders);
        // (name, kind, inode, entry) and (name, kind, inode, place among the folders).
        let mut old: Vec<(&str, Kind, u32, EntryId)> = self
            .run_children(folder)
            .map(|child| {
                let (kind, inode) = self.kind(child);
                (self.name(child), kind, inode, child)
            })
            .collect();
        old.sort_unstable();
        let mut new = Vec::with_capacity(listing.children.len());
        let mut folders = 0;
        for (name, kind, inode) in listing.iter() {
            new.push((name, kind, inode, folders));
            folders += usize::from(kind != Kind::File);
        }
        new.sort_unstable();

        // Anything else than the same name, kind and inode is a removal plus an addition.
        let (mut gone, mut fresh) = (Vec::new(), Vec::new());
        let (mut o, mut n) = (0, 0);
        while o < old.len() || n < new.len() {
            let order = match (old.get(o), new.get(n)) {
                (Some(old), Some(new)) => (old.0, old.1, old.2).cmp(&(new.0, new.1, new.2)),
                (Some(_), None) => Ordering::Less,
                _ => Ordering::Greater,
            };
            if order.is_le() {
                o += 1;
            }
            if order.is_ge() {
                n += 1;
            }
            match order {
                Ordering::Less => {
                    let (name, .., entry) = old[o - 1];
                    if !partial || new.binary_search_by(|new| new.0.cmp(name)).is_ok() {
                        gone.push(entry);
                    }
                }
                Ordering::Greater => fresh.push(new[n - 1]),
                Ordering::Equal => {}
            }
        }

        if !fresh.is_empty() {
            let anchor = added.builder.push("", NO_PARENT, true);
            added.anchors.push(folder);
            for (name, kind, inode, at) in fresh {
                let child = added.builder.push_child(name, anchor, kind, inode);
                if kind != Kind::File {
                    let path = std::mem::take(&mut paths[at]);
                    added.walks.push((path, child, kind == Kind::Passage));
                }
            }
        }
        gone.into_iter().map(|entry| self.remove(entry)).sum()
    }

    /// Removes `entry` and everything inside it from search. Returns how many entries that was.
    fn remove(&mut self, entry: EntryId) -> usize {
        let mut removed = 0;
        let mut stack = vec![entry];
        while let Some(entry) = stack.pop() {
            if self.is_dir(entry) {
                stack.extend(self.run_children(entry));
                self.removed_folders += 1;
            }
            self.entry_parent[entry as usize] = NO_PARENT;
            removed += 1;
        }
        self.removed += removed;
        removed
    }

    /// Appends what an update found, each anchor's children going to its folder in `anchors`.
    /// Returns how many entries were added.
    fn splice(&mut self, new: &Index, anchors: &[EntryId]) -> usize {
        let first_new = self.len();
        // An anchor came first, so name 0 is the anchors' empty name, which is left out.
        debug_assert_eq!(new.name_text(0), "");
        let name_base = self.distinct_names() as NameId - 1;
        let bytes_base = self.names.len() as u32;
        if self.names.capacity() - self.names.len() < new.names.len() {
            self.names
                .reserve_exact(new.names.len().max(self.names.len() / GROW_BY));
        }
        self.names.push_str(&new.names);
        let names = new.distinct_names() - 1;
        grow(&mut self.name_starts, names);
        grow(&mut self.name_letters, names);
        for name in 1..=names {
            self.name_starts
                .push(bytes_base + new.name_starts[name + 1]);
            self.name_letters
                .push(name_fingerprint(new.name_text(name as NameId)));
        }

        let entries = new.len() - anchors.len();
        grow(&mut self.entry_name, entries);
        grow(&mut self.entry_parent, entries);
        grow(&mut self.entry_depth, entries);
        let folders = new.folders.len() - anchors.len();
        grow(&mut self.folders, folders);
        grow(&mut self.folder_inodes, folders);
        let mut in_index = Vec::with_capacity(new.len());
        let mut anchors = anchors.iter();
        let mut folders = new.folders.iter().zip(&new.folder_inodes).peekable();
        let mut passages = new.passages.iter().peekable();
        for (entry, (&name, &parent)) in new.entry_name.iter().zip(&new.entry_parent).enumerate() {
            let entry = entry as EntryId;
            let folder = folders.next_if(|&(&folder, _)| folder == entry);
            if parent == NO_PARENT {
                in_index.push(*anchors.next().expect("one folder per anchor"));
                continue;
            }
            let id = self.len() as EntryId;
            in_index.push(id);
            let parent = in_index[parent as usize];
            self.entry_name.push(name_base + name);
            self.entry_parent.push(parent);
            let depth = self.entry_depth[parent as usize].saturating_add(1);
            self.entry_depth.push(depth);
            if let Some((_, &inode)) = folder {
                self.folders.push(id);
                self.folder_inodes.push(inode);
            }
            if passages.next_if_eq(&&entry).is_some() {
                self.passages.push(id);
            }
        }
        self.add_runs(first_new);
        self.added += entries;
        self.children_apart |= entries > 0;
        entries
    }

    /// Records the runs of children among the entries from `from` on.
    fn add_runs(&mut self, from: usize) {
        let Self {
            runs,
            folders,
            entry_parent,
            ..
        } = self;
        let runs = runs
            .as_mut()
            .expect("runs are made before they are added to");
        runs.first.resize(folders.len(), (0, 0));
        let sorted = runs.more.len();
        let mut current: Option<(EntryId, EntryId, EntryId)> = None;
        for (entry, &parent) in entry_parent.iter().enumerate().skip(from) {
            // Removed entries may sit inside a run without ending it.
            if parent == NO_PARENT {
                continue;
            }
            let entry = entry as EntryId;
            match &mut current {
                Some((folder, _, end)) if *folder == parent => *end = entry + 1,
                _ => {
                    if let Some(run) = current.replace((parent, entry, entry + 1)) {
                        runs.add(folders, run);
                    }
                }
            }
        }
        if let Some(run) = current {
            runs.add(folders, run);
        }
        if runs.more.len() > sorted {
            // A stable sort merges the new runs into the sorted ones in about one pass.
            runs.more.sort();
        }
    }

    /// Drops the removed entries and stores each name once, keeping the order, so searches are
    /// as fast as on a fresh scan. The entry lists are rewritten where they are, so the
    /// rebuild never holds two copies of them.
    fn compact(&mut self) {
        self.name_letters = Vec::new();
        self.entry_depth = Vec::new();
        self.runs = None;
        let mut names = IndexBuilder::new();
        names.reserve_names(self.names.len(), self.distinct_names());
        let mut renamed = vec![NameId::MAX; self.distinct_names()];
        let mut placed = vec![NO_PARENT; self.folders.len()];
        let mut folders = Vec::with_capacity(self.folder_count());
        let mut folder_inodes = Vec::with_capacity(self.folder_count());
        let mut passages = Vec::new();
        let mut passage = self.passages.iter().peekable();
        let (mut slot, mut kept) = (0, 0);
        for entry in 0..self.len() {
            let is_folder = self.folders.get(slot) == Some(&(entry as EntryId));
            slot += usize::from(is_folder);
            let is_passage = passage.next_if_eq(&&(entry as EntryId)).is_some();
            let parent = match self.entry_parent[entry] {
                NO_PARENT if entry == 0 => NO_PARENT,
                NO_PARENT => continue,
                parent => match self.folders.binary_search(&parent) {
                    Ok(at) if placed[at] != NO_PARENT => placed[at],
                    _ => continue,
                },
            };
            let id = kept as EntryId;
            let name = self.entry_name[entry] as usize;
            if renamed[name] == NameId::MAX {
                renamed[name] = names.intern(self.name_text(name as NameId));
            }
            self.entry_name[kept] = renamed[name];
            self.entry_parent[kept] = parent;
            if is_folder {
                placed[slot - 1] = id;
                folders.push(id);
                folder_inodes.push(self.folder_inodes[slot - 1]);
            }
            if is_passage {
                passages.push(id);
            }
            kept += 1;
        }
        drop((renamed, placed));
        for list in [&mut self.entry_name, &mut self.entry_parent] {
            list.truncate(kept);
            list.shrink_to_fit();
        }
        let mut names = names.lists();
        names.names.shrink_to_fit();
        names.name_starts.shrink_to_fit();
        self.names = names.names;
        self.name_starts = names.name_starts;
        self.folders = folders;
        self.folder_inodes = folder_inodes;
        passages.shrink_to_fit();
        self.passages = passages;
        (self.removed, self.removed_folders, self.added) = (0, 0, 0);
        self.add_depths_and_letters();
    }
}

/// Lists grow by a sixteenth at a time, not double as `Vec` does, so a few new files never
/// double the index's memory.
const GROW_BY: usize = 16;

fn grow<T>(list: &mut Vec<T>, more: usize) {
    if list.capacity() - list.len() < more {
        list.reserve_exact(more.max(list.len() / GROW_BY));
    }
}

/// What an update adds, gathered in a builder of its own. Each folder that gains children
/// gets an anchor there: an entry with no parent that stands for the folder.
struct Added {
    builder: IndexBuilder,
    anchors: Vec<EntryId>,
    /// New folders to read in full: path, entry in `builder`, passage.
    walks: Vec<(PathBuf, EntryId, bool)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{is_sound, open_index, save_index};
    use crate::{Access, Clutter, Ignore, Scan, Searcher, Settings};
    use std::collections::BTreeSet;

    /// What search can find: every entry still in the index, by its path below the root.
    fn findable(index: &Index, root: &Path) -> BTreeSet<(PathBuf, (Kind, u32))> {
        (1..index.len() as EntryId)
            .filter(|&entry| index.entry_parent[entry as usize] != NO_PARENT)
            .map(|entry| {
                let path = index.path(entry);
                let below = path.strip_prefix(root).expect("a path up to the root");
                assert_eq!(
                    usize::from(index.entry_depth[entry as usize]),
                    below.components().count().min(255),
                    "{below:?}"
                );
                (below.to_path_buf(), index.kind(entry))
            })
            .collect()
    }

    /// The lists hold together as a saved index must, and every name has its fingerprint.
    fn assert_sound(index: &Index) {
        assert!(is_sound(
            &index.names,
            &index.name_starts,
            &index.entry_name,
            &index.entry_parent,
            &index.folders,
            &index.passages
        ));
        assert_eq!(index.folder_inodes.len(), index.folders.len());
        assert_eq!(index.entry_depth.len(), index.len());
        let letters: Vec<u64> = (0..index.distinct_names())
            .map(|name| name_fingerprint(index.name_text(name as NameId)))
            .collect();
        assert_eq!(index.name_letters, letters);
    }

    /// What deciding whether a change needs a new list reads: each folder's children, and
    /// each folder found by its names from the root.
    fn assert_children_found(index: &Index, root: &Path, context: &str) {
        let mut every = vec![Vec::new(); index.len()];
        for (entry, &parent) in index.entry_parent.iter().enumerate().skip(1) {
            if parent != NO_PARENT {
                every[parent as usize].push(entry as EntryId);
            }
        }
        for &folder in &index.folders {
            if folder != 0 && index.entry_parent[folder as usize] == NO_PARENT {
                continue;
            }
            let expected = &every[folder as usize];
            let children: Vec<EntryId> = index.children(folder).collect();
            assert_eq!(&children, expected, "{context}");
            let folders: Vec<EntryId> = index.child_folders(folder).collect();
            let expected: Vec<EntryId> = expected
                .iter()
                .copied()
                .filter(|&child| index.is_dir(child))
                .collect();
            assert_eq!(folders, expected, "{context}");
            let path = index.path(folder);
            let names = path.strip_prefix(root).unwrap().iter();
            let found = index.find_folder(names.map(|name| name.to_str().unwrap()));
            assert_eq!(found, Some(folder), "{context}: {path:?}");
        }
    }

    fn assert_current(settings: &Settings, index: &Index, root: &Path, context: &str) {
        assert_sound(index);
        assert_children_found(index, root, context);
        let fresh = settings.scan().unwrap().index;
        assert_eq!(findable(index, root), findable(&fresh, root), "{context}");
        assert_eq!(
            (index.entry_count(), index.folder_count()),
            (fresh.len(), fresh.folders.len()),
            "{context}"
        );
    }

    fn write(root: &Path, files: &[&str]) {
        for file in files {
            let file = root.join(file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "").unwrap();
        }
    }

    /// A folder of files nothing touches, so a few changes leave the lists as they are.
    const PADDING: &str = "padding";

    fn pad(root: &Path) {
        fs::create_dir(root.join(PADDING)).unwrap();
        for i in 0..2000 {
            fs::write(root.join(PADDING).join(i.to_string()), "").unwrap();
        }
    }

    fn keep_all(root: &Path) -> Settings {
        Settings::new(root, root, Clutter::Keep, Ignore::default(), Access::Full)
    }

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    /// A file beside the root, named after it, so tests running at once never share one.
    fn index_file(root: &Path) -> PathBuf {
        root.with_extension("index")
    }

    fn search(index: &Index, query: &str) -> Vec<PathBuf> {
        let searcher = Searcher::with_threads(1);
        let hits = searcher.search(index, query, searcher.ticket(), 10);
        let mut found: Vec<PathBuf> = hits
            .unwrap()
            .iter()
            .map(|hit| index.path(hit.entry))
            .collect();
        found.sort();
        found
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, below: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % below as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    /// Names the rules treat differently: clutter, tool downloads and their project files,
    /// a cache tag, an app, ~/Library and its cloud drives, guarded folders, and plain ones.
    const NAMES: &[&str] = &[
        "a",
        "b",
        "c.txt",
        "README.md",
        "Old",
        "keep",
        ".hidden",
        "node_modules",
        "vendor",
        "composer.json",
        "CACHEDIR.TAG",
        "Notes.app",
        "Contents",
        "Library",
        "CloudStorage",
        "Documents",
        "café",
    ];

    /// Everything on disk below `root` but the padding, folders flagged.
    fn on_disk(root: &Path) -> Vec<(PathBuf, bool)> {
        let mut found = vec![(root.to_path_buf(), true)];
        let mut at = 0;
        while at < found.len() {
            let (path, is_dir) = found[at].clone();
            at += 1;
            if is_dir {
                for entry in fs::read_dir(&path).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_name() != PADDING {
                        found.push((entry.path(), entry.file_type().unwrap().is_dir()));
                    }
                }
            }
        }
        found
    }

    /// One random change on disk, and the folders FSEvents would report for it, plus
    /// sometimes the changed item's own path or one below it. No folder is put where one of
    /// `taken` was.
    fn change(rng: &mut Rng, root: &Path, taken: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
        let everything = on_disk(root);
        let folders: Vec<&PathBuf> = everything
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(path, _)| path)
            .collect();
        // The root often, so the special names land where the rules look for them.
        let folder = if rng.below(3) == 0 {
            root
        } else {
            rng.pick(&folders).as_path()
        };
        let target = folder.join(rng.pick(NAMES));
        let item = &everything[rng.below(everything.len())].0;
        let mut changed = vec![folder.to_path_buf()];
        let exists = target.symlink_metadata().is_ok();
        let free = !taken.contains(&target);
        match rng.below(7) {
            0 if !exists => fs::write(&target, "").unwrap(),
            1 if !exists && free => fs::create_dir(&target).unwrap(),
            2 if !exists && free => {
                let deep = target.join(rng.pick(NAMES)).join(rng.pick(NAMES));
                fs::create_dir_all(&deep).unwrap();
                fs::write(deep.join(rng.pick(NAMES)), "").unwrap();
                fs::write(target.join("new.txt"), "").unwrap();
            }
            3 if !exists => {
                let tag = "Signature: 8a477f597d28d172789f06886806bc55";
                fs::write(&target, if rng.below(2) == 0 { tag } else { "" }).unwrap();
            }
            4 if item != root => {
                if item.is_dir() {
                    fs::remove_dir_all(item).unwrap();
                } else {
                    fs::remove_file(item).unwrap();
                }
                changed.push(item.parent().unwrap().to_path_buf());
            }
            // A rename or a move; never into itself.
            5 if item != root && !exists && free && !folder.starts_with(item) => {
                fs::rename(item, &target).unwrap();
                changed.push(item.parent().unwrap().to_path_buf());
            }
            // A file turned into a folder, or the other way round.
            6 if exists && (free || target.is_dir()) => {
                if target.is_dir() {
                    fs::remove_dir_all(&target).unwrap();
                    fs::write(&target, "").unwrap();
                } else {
                    fs::remove_file(&target).unwrap();
                    fs::create_dir(&target).unwrap();
                }
            }
            _ => {}
        }
        match rng.below(4) {
            0 => changed.push(target),
            1 => changed.push(target.join("nowhere").join("deep")),
            2 => changed.push(item.clone()),
            _ => {}
        }
        changed
    }

    /// Batches of random changes, each applied in two updates, must leave the index finding
    /// what a fresh scan finds. With `swaps` off no batch puts a folder where another one
    /// was, the one change only an inode tells apart.
    fn random_changes(swaps: bool, seeds: u64) {
        let rules = "Old\n!~/.hidden/keep\n!~/node_modules/keep\n!~/vendor/keep\n\
                     !~/Notes.app/keep\n!~/Library/keep\n";
        let variants = [
            (Clutter::Keep, "", Access::Full),
            (Clutter::Skip, "", Access::Full),
            (Clutter::Skip, rules, Access::Full),
            (Clutter::Skip, rules, Access::Limited),
        ];
        let (mut compacted, mut in_place) = (0, 0);
        for (variant, &(clutter, rules, access)) in variants.iter().enumerate() {
            for seed in 1..=seeds {
                let (_dir, root) = temp_root();
                // Half the trees are big enough to patch most batches in place.
                if seed % 2 == 0 {
                    pad(&root);
                }
                let rng = &mut Rng(0x9E37_79B9_7F4A_7C15 ^ seed << 8 ^ variant as u64);
                for _ in 0..60 {
                    change(rng, &root, &BTreeSet::new());
                }
                let (ignore, problems) = Ignore::parse(rules, &root, access);
                assert!(problems.is_empty());
                let settings = Settings::new(&root, &root, clutter, ignore, access);
                let mut index = settings.scan().unwrap().index;
                for round in 0..30 {
                    let context = format!("variant {variant}, seed {seed}, round {round}");
                    let taken: BTreeSet<PathBuf> = if swaps {
                        BTreeSet::new()
                    } else {
                        on_disk(&root)
                            .into_iter()
                            .filter_map(|(path, is_dir)| is_dir.then_some(path))
                            .collect()
                    };
                    let mut changed = Vec::new();
                    for _ in 0..=rng.below(6) {
                        changed.extend(change(rng, &root, &taken));
                    }
                    if rng.below(5) == 0 {
                        changed.push(root.parent().unwrap().join("elsewhere"));
                    }
                    // FSEvents may report the folders in any order and in several batches.
                    for at in (1..changed.len()).rev() {
                        changed.swap(at, rng.below(at + 1));
                    }
                    let split = rng.below(changed.len() + 1);
                    for part in [&changed[..split], &changed[split..]] {
                        let live = index.entry_count();
                        let update = settings.update(&mut index, part);
                        assert_eq!(
                            index.entry_count(),
                            live + update.added - update.removed,
                            "{context}"
                        );
                        if update.compacted {
                            compacted += 1;
                        } else if update.added + update.removed > 0 {
                            in_place += 1;
                        }
                    }
                    assert_current(&settings, &index, &root, &context);
                    if rng.below(8) == 0 {
                        let file = index_file(&root);
                        let scan = Scan {
                            index,
                            unreadable_folders: 0,
                        };
                        save_index(&file, &scan, &settings, 0).unwrap();
                        index = open_index(&file, &settings).unwrap().scan.index;
                        fs::remove_file(file).unwrap();
                    }
                }
            }
        }
        assert!(
            compacted > 50 && in_place > 50,
            "rebuilt {compacted} times, patched in place {in_place} times"
        );
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a disk that never hands a new folder the inode of one just deleted"
    )]
    fn after_random_changes_finds_what_a_fresh_scan_finds() {
        random_changes(true, 4);
    }

    #[test]
    fn after_random_changes_that_put_no_folder_in_another_ones_place_finds_the_same() {
        random_changes(false, 2);
    }

    #[test]
    fn a_new_file_is_found_and_a_deleted_one_is_not() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["docs/plan.md", "docs/old-plan.md"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        write(&root, &["docs/new-plan.md"]);
        fs::remove_file(root.join("docs/old-plan.md")).unwrap();

        let update = settings.update(&mut index, &[root.join("docs")]);
        assert_eq!(update.folders_read, 1);
        assert_eq!((update.added, update.removed), (1, 1));
        assert!(!update.compacted);
        assert_eq!(
            search(&index, "plan"),
            [root.join("docs/new-plan.md"), root.join("docs/plan.md")]
        );
        assert_current(&settings, &index, &root, "after the change");
    }

    #[test]
    fn added_names_and_folders_are_searched_without_a_rebuild() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["loose/main.rs"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::create_dir(root.join("project")).unwrap();
        fs::rename(root.join("loose/main.rs"), root.join("project/main.rs")).unwrap();
        write(&root, &["café.txt"]);

        let update = settings.update(&mut index, &[root.clone(), root.join("loose")]);
        assert!(!update.compacted);
        assert_eq!(search(&index, "café"), [root.join("café.txt")]);
        assert_eq!(
            search(&index, "project main"),
            [root.join("project/main.rs")]
        );
        assert_current(&settings, &index, &root, "after the move");
    }

    #[test]
    fn a_moved_folder_leaves_with_everything_inside_it() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(
            &root,
            &["a/project/src/main.rs", "a/project/README.md", "b/x"],
        );
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::rename(root.join("a/project"), root.join("b/project")).unwrap();

        let update = settings.update(&mut index, &[root.join("a"), root.join("b")]);
        assert_eq!((update.added, update.removed), (4, 4));
        assert!(!update.compacted);
        assert_current(&settings, &index, &root, "after the move");
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a disk that never hands a new folder the inode of one just deleted"
    )]
    fn a_folder_replaced_by_one_of_the_same_name_is_read_again() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["a/old.txt"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::remove_dir_all(root.join("a")).unwrap();
        write(&root, &["a/new.txt"]);

        let update = settings.update(&mut index, &[&root]);
        assert_eq!((update.added, update.removed), (2, 2));
        assert_current(&settings, &index, &root, "after the swap");
    }

    #[test]
    fn any_path_on_the_way_reads_the_deepest_folder_still_there() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["a/b/c/d.txt", "x/y.txt"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::remove_dir_all(root.join("a/b")).unwrap();
        write(&root, &["a/new/e.txt", "x/z.txt", "w/v.txt"]);

        // The deleted folder's own path, a file's path inside a new folder, and a ".." that
        // ends the path at the folder before it.
        let changed = [
            root.join("a/b/c"),
            root.join("a/new/e.txt"),
            root.join("x/../w"),
        ];
        let update = settings.update(&mut index, &changed);
        assert_eq!(update.folders_read, 2);
        assert!(!update.compacted);
        let found = findable(&index, &root);
        assert!(
            found
                .iter()
                .any(|(path, _)| path == Path::new("a/new/e.txt"))
        );
        assert!(found.iter().any(|(path, _)| path == Path::new("x/z.txt")));
        assert!(
            !found
                .iter()
                .any(|(path, _)| path.starts_with("a/b") || path.starts_with("w"))
        );

        fs::rename(root.join("x"), root.join("X")).unwrap();
        settings.update(&mut index, &[&root]);
        assert_current(
            &settings,
            &index,
            &root,
            "after a rename that changes only case",
        );
    }

    #[test]
    fn paths_outside_the_root_change_nothing() {
        let (_dir, root) = temp_root();
        write(&root, &["inside/a.txt"]);
        let settings = keep_all(&root.join("inside"));
        let mut index = settings.scan().unwrap().index;
        write(&root, &["inside/b.txt"]);

        let changed = [root.clone(), root.join("insider"), PathBuf::from("inside")];
        assert_eq!(settings.update(&mut index, &changed), Update::default());
        let none: [&Path; 0] = [];
        assert_eq!(settings.update(&mut index, &none), Update::default());
        let mut empty = IndexBuilder::new().finish();
        assert_eq!(
            settings.update(&mut empty, &[root.join("inside")]),
            Update::default()
        );
    }

    #[test]
    fn a_project_file_turns_its_tool_folder_into_the_way_to_a_kept_one() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["site/vendor/lib.php", "site/vendor/keep/k.txt"]);
        let (ignore, _) = Ignore::parse("!~/site/vendor/keep", &root, Access::Full);
        let settings = Settings::new(&root, &root, Clutter::Skip, ignore, Access::Full);
        let mut index = settings.scan().unwrap().index;
        let vendor = |index: &Index| {
            let found = findable(index, &root);
            found
                .iter()
                .find(|(path, _)| path == Path::new("site/vendor"))
                .map(|&(_, (kind, _))| kind)
        };
        assert_eq!(vendor(&index), Some(Kind::Folder));

        write(&root, &["site/composer.json"]);
        assert!(!settings.update(&mut index, &[root.join("site")]).compacted);
        assert_eq!(vendor(&index), Some(Kind::Passage));
        assert_current(&settings, &index, &root, "with composer.json");

        fs::remove_file(root.join("site/composer.json")).unwrap();
        assert!(!settings.update(&mut index, &[root.join("site")]).compacted);
        assert_eq!(vendor(&index), Some(Kind::Folder));
        assert_current(&settings, &index, &root, "without composer.json");
    }

    #[test]
    fn reads_more_folders_than_one_batch_holds() {
        let (_dir, root) = temp_root();
        let folders: Vec<String> = (0..2100).map(|i| format!("many/{i}")).collect();
        let files: Vec<String> = folders.iter().map(|folder| format!("{folder}/a")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        write(&root, &files);
        write(&root, &["site/vendor/keep/k.txt"]);
        let (ignore, _) = Ignore::parse("!~/site/vendor/keep", &root, Access::Full);
        let settings = Settings::new(&root, &root, Clutter::Skip, ignore, Access::Full);
        let mut index = settings.scan().unwrap().index;
        for folder in &folders {
            write(&root, &[&format!("{folder}/b")]);
        }
        // The project file turns vendor into a passage, which drops the folder inside it from
        // the index before that folder's own turn comes.
        write(&root, &["site/composer.json"]);
        let mut changed: Vec<PathBuf> = folders.iter().map(|folder| root.join(folder)).collect();
        changed.extend([root.join("site"), root.join("site/vendor/keep")]);

        let update = settings.update(&mut index, &changed);
        assert_eq!(update.folders_read, 2101);
        assert_current(&settings, &index, &root, "after 2,100 folders");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_it_may_not_read_keeps_what_it_held() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["docs/a.txt", "docs/b.txt", "locked/c.txt"]);
        let lock = |folder: &str, mode| {
            fs::set_permissions(root.join(folder), fs::Permissions::from_mode(mode)).unwrap();
        };
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        let before = findable(&index, &root);
        fs::rename(
            root.join("locked"),
            root.parent().unwrap().join("locked-aside"),
        )
        .unwrap();
        settings.update(&mut index, &[&root]);
        fs::rename(
            root.parent().unwrap().join("locked-aside"),
            root.join("locked"),
        )
        .unwrap();
        lock("docs", 0o000);
        lock("locked", 0o000);

        // docs is read again, and locked is new.
        let update = settings.update(&mut index, &[root.clone(), root.join("docs")]);
        let found = findable(&index, &root);
        lock("docs", 0o755);
        lock("locked", 0o755);
        assert_eq!(update.unreadable_folders, 2);
        assert!(
            found
                .iter()
                .any(|(path, _)| path == Path::new("docs/b.txt"))
        );
        assert!(
            !found
                .iter()
                .any(|(path, _)| path == Path::new("locked/c.txt"))
        );

        settings.update(&mut index, &[root.join("docs"), root.join("locked")]);
        assert_eq!(findable(&index, &root), before);
        assert_current(&settings, &index, &root, "after reading both again");
    }

    #[test]
    fn a_deleted_root_leaves_nothing_and_comes_back() {
        let (_dir, root) = temp_root();
        write(&root, &["a/b.txt", "c.txt"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::remove_dir_all(&root).unwrap();

        let update = settings.update(&mut index, &[root.join("a")]);
        assert_eq!((update.removed, update.unreadable_folders), (3, 0));
        assert_eq!(index.entry_count(), 1);
        assert!(findable(&index, &root).is_empty());

        write(&root, &["d/e.txt"]);
        settings.update(&mut index, &[&root]);
        assert_current(&settings, &index, &root, "after the root came back");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_replaced_by_a_link_to_one_becomes_a_file() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["a/x.txt", "target/t.txt"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        fs::remove_dir_all(root.join("a")).unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("a")).unwrap();

        settings.update(&mut index, &[root.join("a")]);
        let found = findable(&index, &root);
        assert!(found.contains(&(PathBuf::from("a"), (Kind::File, 0))));
        assert_current(&settings, &index, &root, "after the link");
    }

    #[cfg(unix)]
    #[test]
    fn folders_deeper_than_depths_count_still_update() {
        let (_dir, root) = temp_root();
        let deepest: PathBuf = std::iter::repeat_n("d", 260).collect();
        fs::create_dir_all(root.join(&deepest)).unwrap();
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        write(&root, &[deepest.join("f").to_str().unwrap()]);

        let update = settings.update(&mut index, &[root.join(&deepest)]);
        assert_eq!((update.folders_read, update.added), (1, 1));
        assert_current(&settings, &index, &root, "260 folders down");
    }

    #[test]
    fn compacting_drops_removed_entries_and_repeated_names() {
        let (_dir, root) = temp_root();
        let files: Vec<String> = (0..200).map(|i| format!("f{i}/README.md")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        write(&root, &files);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        let fresh = || settings.scan().unwrap().index;

        // Few enough changes to leave the lists as they are. The new names are stored again.
        fs::remove_dir_all(root.join("f0")).unwrap();
        write(&root, &["g/README.md"]);
        let update = settings.update(&mut index, &[&root]);
        assert!(!update.compacted);
        assert_eq!(index.len(), fresh().len() + 2);
        assert_eq!(index.distinct_names(), fresh().distinct_names() + 2);

        for i in 1..30 {
            fs::remove_dir_all(root.join(format!("f{i}"))).unwrap();
        }
        let update = settings.update(&mut index, &[&root]);
        assert!(update.compacted);
        let fresh = fresh();
        assert_eq!(index.len(), fresh.len());
        assert_eq!(index.distinct_names(), fresh.distinct_names());
        assert!(index.runs.is_none());
        assert_current(&settings, &index, &root, "after compacting");
    }

    /// The root, a folder and 318 files: 320 entries, so 10 removed or added entries sit on
    /// the line and 11 pass it.
    fn at_the_line() -> (tempfile::TempDir, PathBuf, Settings, Index) {
        let (dir, root) = temp_root();
        let files: Vec<String> = (0..318).map(|i| format!("f/{i}")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        write(&root, &files);
        let settings = keep_all(&root);
        let index = settings.scan().unwrap().index;
        assert_eq!(index.len(), 320);
        (dir, root, settings, index)
    }

    #[test]
    fn rebuilds_only_past_the_line() {
        let (_dir, root, settings, mut index) = at_the_line();
        for i in 0..10 {
            fs::remove_file(root.join(format!("f/{i}"))).unwrap();
        }
        assert!(!settings.update(&mut index, &[root.join("f")]).compacted);
        fs::remove_file(root.join("f/10")).unwrap();
        assert!(settings.update(&mut index, &[root.join("f")]).compacted);
        assert_current(&settings, &index, &root, "after the rebuild");
    }

    #[test]
    fn added_entries_count_toward_a_rebuild_after_saving_and_opening() {
        let (_dir, root, settings, mut index) = at_the_line();
        let file = index_file(&root);
        // 6 added of 326 stays under the line, 11 of 331 passes it.
        for (from, to) in [(0, 6), (6, 11)] {
            for i in from..to {
                write(&root, &[&format!("f/new{i}")]);
            }
            let update = settings.update(&mut index, &[root.join("f")]);
            assert_eq!(update.compacted, to == 11);
            let scan = Scan {
                index,
                unreadable_folders: 0,
            };
            save_index(&file, &scan, &settings, 0).unwrap();
            index = open_index(&file, &settings).unwrap().scan.index;
        }
        fs::remove_file(file).unwrap();
        assert_eq!(
            index.distinct_names(),
            settings.scan().unwrap().index.distinct_names()
        );
    }

    /// Each list's length and room.
    fn lists(index: &Index) -> [(usize, usize); 8] {
        [
            (index.names.len(), index.names.capacity()),
            (index.name_starts.len(), index.name_starts.capacity()),
            (index.name_letters.len(), index.name_letters.capacity()),
            (index.entry_name.len(), index.entry_name.capacity()),
            (index.entry_parent.len(), index.entry_parent.capacity()),
            (index.entry_depth.len(), index.entry_depth.capacity()),
            (index.folders.len(), index.folders.capacity()),
            (index.folder_inodes.len(), index.folder_inodes.capacity()),
        ]
    }

    #[test]
    fn a_few_new_files_grow_each_list_by_a_sixteenth_at_most() {
        let (_dir, root) = temp_root();
        let files: Vec<String> = (0..2000).map(|i| format!("d{}/{i}.txt", i % 50)).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        write(&root, &files);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        settings.update(&mut index, &[&root]);

        write(&root, &["d1/new.txt"]);
        assert_eq!(settings.update(&mut index, &[root.join("d1")]).added, 1);
        let grown = lists(&index);
        for (len, capacity) in grown {
            assert!(
                capacity - len <= len / GROW_BY + 1,
                "{len} long, room for {capacity}"
            );
        }
        write(&root, &["d1/newer.txt"]);
        assert_eq!(settings.update(&mut index, &[root.join("d1")]).added, 1);
        let capacities = |lists: [(usize, usize); 8]| lists.map(|(_, capacity)| capacity);
        assert_eq!(capacities(lists(&index)), capacities(grown));
    }

    #[test]
    fn finds_every_child_however_the_runs_fall() {
        for seed in 1..=200u64 {
            let rng = &mut Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut builder = IndexBuilder::new();
            let mut folders = vec![builder.push("/r", NO_PARENT, true)];
            for _ in 0..rng.below(40) {
                let is_dir = rng.below(3) == 0;
                let entry = builder.push("x", *rng.pick(&folders), is_dir);
                if is_dir {
                    folders.push(entry);
                }
            }
            let mut index = builder.finish();
            index.runs = Some(Runs::default());
            index.add_runs(0);
            for step in 0..6 {
                for _ in 0..rng.below(4) {
                    let entry = 1 + rng.below(index.len());
                    if entry < index.len() {
                        index.entry_parent[entry] = NO_PARENT;
                    }
                }
                // Appended children, often several to one folder in a row, and new folders.
                let first_new = index.len();
                let mut parent = *rng.pick(&index.folders);
                for _ in 0..rng.below(12) {
                    if rng.below(2) == 0 {
                        parent = *rng.pick(&index.folders);
                    }
                    let id = index.len() as EntryId;
                    index.entry_name.push(0);
                    index.entry_parent.push(parent);
                    if rng.below(3) == 0 {
                        index.folders.push(id);
                        index.folder_inodes.push(0);
                    }
                }
                index.add_runs(first_new);
                for &folder in &index.folders {
                    let mut children: Vec<EntryId> = index.run_children(folder).collect();
                    children.sort_unstable();
                    let expected: Vec<EntryId> = (0..index.len() as EntryId)
                        .filter(|&entry| index.entry_parent[entry as usize] == folder)
                        .collect();
                    assert_eq!(children, expected, "seed {seed}, step {step}");
                }
            }
        }
    }
}
