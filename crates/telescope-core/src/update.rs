//! Keeping the index current: each changed folder is read again and the lists are patched in
//! place, so a change costs a few folder reads instead of a rescan.

use crate::index::{EntryId, Index, IndexBuilder, NO_PARENT, NameId, name_fingerprint};
use crate::siblings::{NO_FOLDER, Siblings};
use crate::walk::{Kind, Listing, Rules, walk};
use hashbrown::HashSet;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
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

/// What an update changes while it runs, folded into the runs of siblings when it ends, so it
/// costs a step per run and per change rather than a list as long as the index.
struct Edit {
    /// Each folder's runs of children. After an update a folder's children may sit in several
    /// runs, and finding them in the runs alone would read every run after the folder.
    runs: Runs,
    /// Entries this update removed.
    removed: HashSet<EntryId>,
    /// The folder of each entry this update added, in order.
    added_parents: Vec<EntryId>,
}

/// Each folder's runs, by the folder's place in `folders`.
struct Runs {
    first: Vec<u32>,
    /// Further runs as (folder place, run), by folder place.
    more: Vec<(u32, u32)>,
}

const NO_RUN: u32 = u32::MAX;

impl Runs {
    fn new(siblings: &Siblings, folders: usize) -> Self {
        let mut runs = Self {
            first: vec![NO_RUN; folders],
            more: Vec::new(),
        };
        for (run, &slot) in siblings.folders.iter().enumerate() {
            if slot == NO_FOLDER {
                continue;
            }
            let first = &mut runs.first[slot as usize];
            if *first == NO_RUN {
                *first = run as u32;
            } else {
                runs.more.push((slot, run as u32));
            }
        }
        runs.more.sort_unstable();
        runs
    }

    fn of(&self, slot: usize) -> impl Iterator<Item = usize> + '_ {
        let from = self.more.partition_point(|&(s, _)| (s as usize) < slot);
        let more = self.more[from..]
            .iter()
            .take_while(move |&&(s, _)| s as usize == slot)
            .map(|&(_, run)| run);
        std::iter::once(self.first[slot])
            .filter(|&run| run != NO_RUN)
            .chain(more)
            .map(|run| run as usize)
    }
}

impl Index {
    /// Reads each changed folder again and brings its part of the index up to date: removed
    /// children leave with everything inside them, new ones are added with everything the
    /// rules keep inside them, and a move is a removal plus an addition. A path that is not
    /// an indexed folder updates the deepest indexed folder on its way, so a file's own path,
    /// a new folder's or a deleted one's all work. A path under no root changes nothing.
    pub(crate) fn update<P: AsRef<Path>>(&mut self, rules: &Rules, changed: &[P]) -> Update {
        let mut update = Update::default();
        // Copied, as the lists change below while these paths are still read.
        let roots: Vec<(EntryId, PathBuf)> = self
            .roots
            .iter()
            .map(|&root| (root, PathBuf::from(self.name(root))))
            .collect();
        let changed: Vec<&Path> = changed.iter().map(AsRef::as_ref).collect();
        let under_a_root = |path: &&Path| roots.iter().any(|(_, root)| path.starts_with(root));
        if !changed.iter().any(under_a_root) {
            return update;
        }
        let first_new = self.len();
        let mut edit = self.edit();
        let mut targets = self.changed_folders(&edit, &roots, &changed);
        // Parents first, so a folder its parent replaced is not read into the old one.
        targets.sort_unstable_by_key(|&(folder, ..)| folder);
        targets.dedup_by_key(|&mut (folder, ..)| folder);
        let mut added = Added {
            builder: IndexBuilder::new(),
            anchors: Vec::new(),
            walks: Vec::new(),
        };
        for chunk in targets.chunks(READ_CHUNK) {
            let listings: Vec<(Listing, bool)> = chunk
                .par_iter()
                .map_init(PathBuf::new, |at, &(folder, depth, top, below)| {
                    folder_path(at, &roots[top].1, below, depth);
                    let passage = self.passages.binary_search(&folder).is_ok();
                    let listing = rules.list(at, self.is_root(folder), passage);
                    // A folder deleted since it was found lost everything in it.
                    let partial = listing.unreadable
                        && fs::symlink_metadata(&at).is_ok_and(|meta| meta.is_dir());
                    (listing, partial)
                })
                .collect();
            for (&(folder, ..), (listing, partial)) in chunk.iter().zip(listings) {
                if edit.removed.contains(&folder) {
                    continue;
                }
                update.folders_read += 1;
                update.unreadable_folders += usize::from(partial);
                update.removed += self.apply(&mut edit, folder, listing, partial, &mut added);
            }
        }
        if !added.anchors.is_empty() {
            let walked = walk(added.builder, rules, NO_PARENT, added.walks, &|| false);
            update.unreadable_folders += walked.unreadable_folders;
            let new = walked.builder.lists();
            update.added = self.splice(&mut edit.added_parents, &new, &added.anchors);
        }
        self.fold_in(edit, first_new);
        update.compacted = self.compact_if_due();
        update
    }

    /// Folds what `edit` changed into the runs of siblings; entries from `first_new` on are new.
    fn fold_in(&mut self, edit: Edit, first_new: usize) {
        if edit.removed.is_empty() && edit.added_parents.is_empty() {
            return;
        }
        let mut removed: Vec<EntryId> = edit.removed.into_iter().collect();
        removed.sort_unstable();
        let added = &edit.added_parents;
        self.siblings = self
            .siblings
            .edited(&removed, first_new, added, &self.folders);
    }

    /// Rebuilds the lists once the entries removed, and the names added entries store again,
    /// pass their share. Returns whether it did.
    fn compact_if_due(&mut self) -> bool {
        let due = (self.removed + self.added) * COMPACT_SHARE > self.len();
        if due {
            let mut parents = self.parents();
            self.compact(&mut parents);
            self.siblings = Siblings::from_parents(&parents, &self.folders);
        }
        due
    }

    /// Scans `volume`, a real path, as one more root, keeping what `rules` keep inside it,
    /// without reading any other root again. Fails with the system's own error when the
    /// volume cannot be opened, with `AlreadyExists` when it is, holds or sits inside a root
    /// already indexed, and with `InvalidInput` for an index with no root to add it beside.
    /// A volume gone by the end of the scan, as one unplugged midway, fails with `NotFound`,
    /// and one `stop` asked to leave fails with `Interrupted`; neither leaves anything behind.
    pub(crate) fn add_volume(
        &mut self,
        volume: &Path,
        rules: &Rules,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<Update> {
        if self.roots.is_empty() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let overlaps = self.roots.iter().any(|&root| {
            let root = Path::new(self.name(root));
            volume.starts_with(root) || root.starts_with(volume)
        });
        if overlaps {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        fs::read_dir(volume)?;
        let mut builder = IndexBuilder::new();
        let anchor = builder.push("", NO_PARENT, true);
        let folders = vec![(volume.to_path_buf(), anchor, false)];
        let walked = walk(builder, rules, anchor, folders, stop);
        if stop() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if !fs::symlink_metadata(volume).is_ok_and(|meta| meta.is_dir()) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let new = walked.builder.lists();
        let name = volume.to_string_lossy();
        self.reserve_exact(name.len(), &new);
        // Appended whole, each folder's children side by side as a scan leaves them, so
        // nothing needs compacting and searches stay as fast as on a fresh scan.
        let (added, apart) = (self.added, self.children_apart);
        let first_new = self.len();
        let root = self.push_root(&name);
        let mut parents = Vec::with_capacity(new.len());
        parents.push(NO_PARENT);
        let entries = self.splice(&mut parents, &new, &[root]);
        self.siblings = self
            .siblings
            .edited(&[], first_new, &parents, &self.folders);
        (self.added, self.children_apart) = (added, apart);
        Ok(Update {
            folders_read: 1,
            added: 1 + entries,
            unreadable_folders: walked.unreadable_folders,
            ..Update::default()
        })
    }

    /// Room for a root named with `name_bytes` and what `new` holds below it, and no more:
    /// the lists usually grow by a sixteenth, which a whole volume needs not.
    fn reserve_exact(&mut self, name_bytes: usize, new: &Index) {
        let names = new.distinct_names();
        let (entries, folders) = (new.len(), new.folders.len());
        self.names.reserve_exact(name_bytes + new.names.len());
        self.name_starts.reserve_exact(names);
        self.name_letters.reserve_exact(names);
        self.name_digit_starts.reserve_exact(names);
        self.name_symbols.reserve_exact(names);
        self.entry_name.reserve_exact(entries);
        self.folders.reserve_exact(folders);
        self.folder_inodes.reserve_exact(folders);
        self.passages.reserve_exact(new.passages.len());
        self.roots.reserve_exact(1);
    }

    /// Takes a volume and everything inside it out of search. `None` when `volume` is not a
    /// volume this index holds.
    pub(crate) fn remove_volume(&mut self, volume: &Path) -> Option<Update> {
        let root = self.root_at(volume).filter(|&root| root != 0)?;
        let mut edit = self.edit();
        self.roots.retain(|&kept| kept != root);
        let removed = self.remove(&mut edit, root);
        self.fold_in(edit, self.len());
        Some(Update {
            removed,
            compacted: self.compact_if_due(),
            ..Update::default()
        })
    }

    /// Appends a root, its name stored like any name an update adds, in room `reserve_exact`
    /// made. Its run is left for `Siblings::edited`.
    fn push_root(&mut self, name: &str) -> EntryId {
        let root = self.len() as EntryId;
        let name_id = self.distinct_names() as NameId;
        self.names.push_str(name);
        // A path is at most 1,024 bytes, so it fits a group beside 15 file names.
        assert!(
            self.name_starts.push(self.names.len() as u32),
            "a volume's path fits a group"
        );
        let (letters, starts, symbols) = name_fingerprint(name);
        self.name_letters.push(letters);
        self.name_digit_starts.push(starts);
        self.name_symbols.push(symbols);
        self.entry_name.push(name_id);
        self.folders.push(root);
        self.folder_inodes.push(0);
        self.roots.push(root);
        root
    }

    /// For each changed folder, the deepest indexed folder on its path that is still a folder
    /// on disk, how many names below its root lead to it, that root's place in `roots`, and
    /// one such path. A path under no root has none. Nothing is copied per path, as a
    /// catch-up after a restart may bring many thousands.
    fn changed_folders<'p>(
        &self,
        edit: &Edit,
        roots: &[(EntryId, PathBuf)],
        changed: &[&'p Path],
    ) -> Vec<(EntryId, usize, usize, &'p Path)> {
        let mut found = Vec::new();
        // Roots never sit inside one another, so each path is below one root at most.
        for (top, (root, root_path)) in roots.iter().enumerate() {
            // Each path below the root, beside what is left of it below the folder reached so far.
            let mut paths: Vec<(&Path, &Path)> = changed
                .iter()
                .filter_map(|path| path.strip_prefix(root_path).ok())
                .map(|below| (below, below))
                .collect();
            // `Path` sorts name by name, so paths into one folder sit together and its children
            // are looked up once for them all.
            paths.sort_unstable_by_key(|&(below, _)| below);
            self.descend(edit, *root, 0, top, &mut paths, &mut found);
        }
        found
            .into_par_iter()
            .map_init(PathBuf::new, |at, (mut folder, mut depth, top, below)| {
                folder_path(at, &roots[top].1, below, depth);
                // A folder deleted or turned into a file is read from its parent, which lost it.
                while !self.is_root(folder)
                    && !fs::symlink_metadata(&at).is_ok_and(|meta| meta.is_dir())
                {
                    folder = self.parent(folder);
                    at.pop();
                    depth -= 1;
                }
                (folder, depth, top, below)
            })
            .collect()
    }

    /// Finds the folder for each of `paths`, which are sorted and all lead through `folder`,
    /// `depth` names below root number `top`. Each folder is found once, however many paths
    /// lead to it.
    fn descend<'p>(
        &self,
        edit: &Edit,
        folder: EntryId,
        depth: usize,
        top: usize,
        paths: &mut [(&'p Path, &'p Path)],
        found: &mut Vec<(EntryId, usize, usize, &'p Path)>,
    ) {
        let ends = paths.partition_point(|&(_, rest)| first_name(rest).is_none());
        let mut here = paths[..ends].first().map(|&(below, _)| below);
        let paths = &mut paths[ends..];
        if !paths.is_empty() {
            let mut children: Vec<(&str, EntryId)> = self
                .run_children(edit, folder)
                .filter(|&child| self.is_dir(child))
                .map(|child| (self.name(child), child))
                .collect();
            children.sort_unstable();
            for group in paths.chunk_by_mut(|a, b| first_name(a.1) == first_name(b.1)) {
                let name = first_name(group[0].1).unwrap_or_default().to_string_lossy();
                match children.binary_search_by(|&(child, _)| child.cmp(&name)) {
                    Ok(at) => {
                        for (_, rest) in group.iter_mut() {
                            let mut parts = rest.components();
                            parts.next();
                            *rest = parts.as_path();
                        }
                        self.descend(edit, children[at].1, depth + 1, top, group, found);
                    }
                    Err(_) => here = here.or(Some(group[0].0)),
                }
            }
        }
        if let Some(below) = here {
            found.push((folder, depth, top, below));
        }
    }

    fn run_children<'a>(
        &'a self,
        edit: &'a Edit,
        folder: EntryId,
    ) -> impl Iterator<Item = EntryId> + 'a {
        let len = self.len();
        let slot = self.folders.binary_search(&folder).ok();
        slot.into_iter()
            .flat_map(|slot| edit.runs.of(slot))
            .flat_map(move |run| self.siblings.entries(run, len))
            .map(|child| child as EntryId)
            .filter(|child| !edit.removed.contains(child))
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
        edit: &mut Edit,
        folder: EntryId,
        mut listing: Listing,
        partial: bool,
        added: &mut Added,
    ) -> usize {
        let mut paths = std::mem::take(&mut listing.folders);
        // (name, kind, inode, entry) and (name, kind, inode, place among the folders).
        let mut old: Vec<(&str, Kind, u32, EntryId)> = self
            .run_children(edit, folder)
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
        gone.into_iter().map(|entry| self.remove(edit, entry)).sum()
    }

    /// Removes `entry` and everything inside it from search. Returns how many entries that was.
    fn remove(&mut self, edit: &mut Edit, entry: EntryId) -> usize {
        let mut removed = 0;
        let mut stack = vec![entry];
        while let Some(entry) = stack.pop() {
            if self.is_dir(entry) {
                stack.extend(self.run_children(edit, entry));
                self.removed_folders += 1;
            }
            edit.removed.insert(entry);
            removed += 1;
        }
        self.removed += removed;
        removed
    }

    /// Appends what an update found, each anchor's children going to its folder in `anchors`,
    /// and each added entry's folder to `added_parents`. Returns how many entries were added.
    fn splice(
        &mut self,
        added_parents: &mut Vec<EntryId>,
        new: &Index,
        anchors: &[EntryId],
    ) -> usize {
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
        let groups = self.name_starts.more_groups(names);
        grow(&mut self.name_starts.groups, groups);
        grow(&mut self.name_starts.offsets, names);
        grow(&mut self.name_letters, names);
        grow(&mut self.name_digit_starts, names);
        grow(&mut self.name_symbols, names);
        for name in 1..=names {
            let end = bytes_base + new.name_starts.get(name + 1) as u32;
            assert!(
                self.name_starts.push(end),
                "a file name is at most 765 bytes"
            );
            let (letters, starts, symbols) = name_fingerprint(new.name_text(name as NameId));
            self.name_letters.push(letters);
            self.name_digit_starts.push(starts);
            self.name_symbols.push(symbols);
        }

        let entries = new.len() - anchors.len();
        grow(&mut self.entry_name, entries);
        added_parents.reserve_exact(entries);
        let folders = new.folders.len() - anchors.len();
        grow(&mut self.folders, folders);
        grow(&mut self.folder_inodes, folders);
        let mut in_index = Vec::with_capacity(new.len());
        let mut anchors = anchors.iter();
        let mut folders = new.folders.iter().zip(&new.folder_inodes).peekable();
        let mut passages = new.passages.iter().peekable();
        for (entry, (&name, parent)) in new.entry_name.iter().zip(new.parents()).enumerate() {
            let entry = entry as EntryId;
            let folder = folders.next_if(|&(&folder, _)| folder == entry);
            if parent == NO_PARENT {
                in_index.push(*anchors.next().expect("one folder per anchor"));
                continue;
            }
            let id = self.len() as EntryId;
            in_index.push(id);
            self.entry_name.push(name_base + name);
            added_parents.push(in_index[parent as usize]);
            if let Some((_, &inode)) = folder {
                self.folders.push(id);
                self.folder_inodes.push(inode);
            }
            if passages.next_if_eq(&&entry).is_some() {
                self.passages.push(id);
            }
        }
        self.added += entries;
        self.children_apart |= entries > 0;
        entries
    }

    fn edit(&self) -> Edit {
        Edit {
            runs: Runs::new(&self.siblings, self.folders.len()),
            removed: HashSet::new(),
            added_parents: Vec::new(),
        }
    }

    /// Drops the removed entries and stores each name once, keeping the order, so searches are
    /// as fast as on a fresh scan. The entry lists are rewritten where they are, so the
    /// rebuild never holds two copies of them.
    fn compact(&mut self, parents: &mut Vec<EntryId>) {
        self.name_letters = Vec::new();
        self.name_digit_starts = Vec::new();
        self.name_symbols = Vec::new();
        self.siblings = Siblings::default();
        let mut names = IndexBuilder::new();
        names.reserve_names(self.names.len(), self.distinct_names());
        let mut renamed = vec![NameId::MAX; self.distinct_names()];
        let mut placed = vec![NO_PARENT; self.folders.len()];
        let mut folders = Vec::with_capacity(self.folder_count());
        let mut folder_inodes = Vec::with_capacity(self.folder_count());
        let mut passages = Vec::new();
        let mut roots = Vec::with_capacity(self.roots.len());
        let mut passage = self.passages.iter().peekable();
        let (mut slot, mut kept) = (0, 0);
        for entry in 0..self.len() {
            let is_folder = self.folders.get(slot) == Some(&(entry as EntryId));
            slot += usize::from(is_folder);
            let is_passage = passage.next_if_eq(&&(entry as EntryId)).is_some();
            let parent = match parents[entry] {
                NO_PARENT if self.is_root(entry as EntryId) => {
                    roots.push(kept as EntryId);
                    NO_PARENT
                }
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
            parents[kept] = parent;
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
        for list in [&mut self.entry_name, parents] {
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
        self.roots = roots;
        (self.removed, self.removed_folders, self.added) = (0, 0, 0);
        self.add_letters();
    }
}

/// The names on the way down from the root, up to anything else, such as a "..".
fn names(below: &Path) -> impl Iterator<Item = &OsStr> {
    below.components().map_while(|part| match part {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}

fn first_name(path: &Path) -> Option<&OsStr> {
    names(path).next()
}

/// Sets `at` to the folder `depth` names down `below` from the root.
fn folder_path(at: &mut PathBuf, root: &Path, below: &Path, depth: usize) {
    at.as_mut_os_string().clear();
    at.push(root);
    at.extend(names(below).take(depth));
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

    /// The root `path` is under, and its entry.
    fn top_of<'i>(index: &'i Index, path: &Path) -> (&'i Path, EntryId) {
        let mut tops = index.roots.iter().map(|&r| (Path::new(index.name(r)), r));
        tops.find(|(top, _)| path.starts_with(top))
            .expect("a path up to a root")
    }

    /// What search can find: every entry still in the index, by its path below `root`, or
    /// its whole path on a volume.
    fn findable(index: &Index, root: &Path) -> BTreeSet<(PathBuf, (Kind, u32))> {
        (0..index.len() as EntryId)
            .filter(|&entry| index.parent(entry) != NO_PARENT)
            .map(|entry| {
                let path = index.path(entry);
                let below = path.strip_prefix(top_of(index, &path).0).unwrap();
                assert_eq!(
                    usize::from(index.depth(entry)),
                    below.components().count().min(255),
                    "{below:?}"
                );
                let shown = path.strip_prefix(root).unwrap_or(&path);
                (shown.to_path_buf(), index.kind(entry))
            })
            .collect()
    }

    /// The lists hold together as a saved index must, and every name has its fingerprint.
    fn assert_sound(index: &Index) {
        let starts: Vec<u32> = index.name_starts.iter().collect();
        assert!(is_sound(
            &index.names,
            &starts,
            &index.entry_name,
            (&index.siblings.starts, &index.siblings.folders),
            &index.folders,
            &index.passages,
            &index.roots
        ));
        assert_eq!(index.folder_inodes.len(), index.folders.len());
        let runs = Siblings::from_parents(&index.parents(), &index.folders);
        assert_eq!(index.siblings, runs);
        let fingerprints: Vec<(u64, u8, u8)> = (0..index.distinct_names())
            .map(|name| name_fingerprint(index.name_text(name as NameId)))
            .collect();
        let kept = (index.name_letters.iter().zip(&index.name_digit_starts))
            .zip(&index.name_symbols)
            .map(|((&letters, &starts), &symbols)| (letters, starts, symbols));
        assert_eq!(kept.collect::<Vec<_>>(), fingerprints);
    }

    /// What deciding whether a change needs a new list reads: each folder's children, and
    /// each folder found by its names from its root.
    fn assert_children_found(index: &Index, context: &str) {
        let mut every = vec![Vec::new(); index.len()];
        for (entry, &parent) in index.parents().iter().enumerate() {
            if parent != NO_PARENT {
                every[parent as usize].push(entry as EntryId);
            }
        }
        for &folder in &index.folders {
            if index.is_removed(folder) {
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
            let (top, top_entry) = top_of(index, &path);
            let names = path.strip_prefix(top).unwrap().iter();
            let found = index.find_folder(top_entry, names.map(|name| name.to_str().unwrap()));
            assert_eq!(found, Some(folder), "{context}: {path:?}");
        }
    }

    /// What a fresh scan finds, with the same volumes added.
    fn fresh(settings: &Settings, volumes: &[PathBuf]) -> Index {
        let mut fresh = settings.scan().unwrap().index;
        for volume in volumes {
            settings.add_volume(&mut fresh, volume).unwrap();
        }
        fresh
    }

    fn assert_current(settings: &Settings, index: &Index, root: &Path, context: &str) {
        assert_sound(index);
        assert_children_found(index, context);
        let volumes: Vec<PathBuf> = index.volumes().map(Path::to_path_buf).collect();
        let fresh = fresh(settings, &volumes);
        assert_eq!(findable(index, root), findable(&fresh, root), "{context}");
        assert_eq!(
            (index.entry_count(), index.folder_count()),
            (fresh.entry_count(), fresh.folder_count()),
            "{context}"
        );
        // Words under four letters, as a longer one may take a typo only when no name holds
        // it, and the names of removed entries are kept until the lists are rebuilt.
        for query in ["a", "md re", "a !b", "c kee", "b !old", "doc lib"] {
            assert_eq!(
                hits(index, query),
                hits(&fresh, query),
                "{context}: {query:?}"
            );
        }
    }

    /// Every hit for `query`, by path and score.
    fn hits(index: &Index, query: &str) -> Vec<(PathBuf, u32)> {
        let searcher = Searcher::with_threads(1);
        let apps = crate::Apps::default();
        let hits = searcher.search(index, &apps, query, searcher.ticket(), index.len());
        let mut found: Vec<(PathBuf, u32)> = (hits.unwrap().iter())
            .map(|hit| (index.path(hit.entry().unwrap()), hit.score))
            .collect();
        found.sort();
        found
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
        let hits = searcher.search(index, &crate::Apps::default(), query, searcher.ticket(), 10);
        let mut found: Vec<PathBuf> = hits
            .unwrap()
            .iter()
            .map(|hit| index.path(hit.entry().unwrap()))
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
    /// was, the one change only an inode tells apart. Every third tree has two volumes beside
    /// it, changed too, and now and then ejected and plugged in again.
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
                // Volumes are read only with Full Disk Access.
                let (_volume_dirs, volumes): (Vec<_>, Vec<_>) = (0..2).map(|_| temp_root()).unzip();
                let with_volumes = seed % 3 == 1 && access == Access::Full;
                for volume in &volumes {
                    for _ in 0..30 {
                        change(rng, volume, &BTreeSet::new());
                    }
                }
                let (ignore, problems) = Ignore::parse(rules, &root, access);
                assert!(problems.is_empty());
                let settings = Settings::new(&root, &root, clutter, ignore, access);
                let mut index = settings.scan().unwrap().index;
                if with_volumes {
                    for volume in &volumes {
                        settings.add_volume(&mut index, volume).unwrap();
                    }
                }
                for round in 0..30 {
                    let context = format!("variant {variant}, seed {seed}, round {round}");
                    let taken = |top: &Path| -> BTreeSet<PathBuf> {
                        if swaps {
                            return BTreeSet::new();
                        }
                        on_disk(top)
                            .into_iter()
                            .filter_map(|(path, is_dir)| is_dir.then_some(path))
                            .collect()
                    };
                    let tops = [&root, &volumes[0], &volumes[1]];
                    let taken = tops.map(|top| taken(top));
                    let mut changed = Vec::new();
                    for _ in 0..=rng.below(6) {
                        let top = if with_volumes { rng.below(3) } else { 0 };
                        changed.extend(change(rng, tops[top], &taken[top]));
                    }
                    // Ejected or plugged in again, with changes on it still to come.
                    for volume in volumes.iter().filter(|_| with_volumes && rng.below(8) == 0) {
                        let (live, held) = (index.entry_count(), index.volumes().count());
                        match settings.remove_volume(&mut index, volume) {
                            Some(ejected) => {
                                assert_eq!(index.entry_count(), live - ejected.removed);
                                assert_eq!(index.volumes().count(), held - 1, "{context}");
                            }
                            None => {
                                let plugged = settings.add_volume(&mut index, volume).unwrap();
                                assert_eq!(index.entry_count(), live + plugged.added);
                                assert_eq!(index.volumes().count(), held + 1, "{context}");
                            }
                        }
                    }
                    let roots: Vec<&Path> = std::iter::once(root.as_path())
                        .chain(index.volumes())
                        .collect();
                    for (at, a) in roots.iter().enumerate() {
                        for b in &roots[at + 1..] {
                            assert!(!a.starts_with(b) && !b.starts_with(a), "{context}");
                        }
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
                        let opened = open_index(&file, &settings).unwrap().scan.index;
                        fs::remove_file(file).unwrap();
                        // Counted again from the lists, as an update counted them on the way.
                        let counts =
                            |i: &Index| (i.removed, i.removed_folders, i.added, i.roots.clone());
                        assert_eq!(counts(&opened), counts(&scan.index), "{context}");
                        index = opened;
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
        // ends the path at the folder before it. Joining would drop the ".." on Windows.
        let mut dotted = root.clone().into_os_string();
        dotted.push(format!("{0}x{0}..{0}w", std::path::MAIN_SEPARATOR));
        let changed = [
            root.join("a/b/c"),
            root.join("a/new/e.txt"),
            PathBuf::from(dotted),
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
    fn many_paths_into_one_folder_find_it_once() {
        let (_dir, root) = temp_root();
        write(&root, &["a/b/c.txt", "a/d.txt", "a-b/g.txt", "e/f.txt"]);
        let settings = keep_all(&root);
        let index = settings.scan().unwrap().index;
        let edit = index.edit();
        let mut changed: Vec<PathBuf> = (0..50)
            .flat_map(|i| [format!("a/b/new{i}.txt"), format!("a/gone{i}/x")])
            .map(|path| root.join(path))
            .collect();
        let paths = ["a/b", "e", "a-b/z/q", "a/b/c.txt", "a/b", "a-b"];
        changed.extend(paths.map(|path| root.join(path)));
        let mut dotted = root.join("a").into_os_string();
        dotted.push(format!("{0}..{0}e", std::path::MAIN_SEPARATOR));
        changed.push(PathBuf::from(dotted));
        let changed: Vec<&Path> = changed.iter().map(PathBuf::as_path).collect();
        let mut found: Vec<(&str, usize)> = index
            .changed_folders(&edit, &[(0, root.clone())], &changed)
            .into_iter()
            .map(|(folder, depth, ..)| (index.name(folder), depth))
            .collect();
        found.sort_unstable();
        assert_eq!(found, [("a", 1), ("a-b", 1), ("b", 2), ("e", 1)]);
    }

    #[test]
    fn paths_outside_the_root_change_nothing() {
        let (_dir, root) = temp_root();
        write(&root, &["inside/a.txt"]);
        let settings = keep_all(&root.join("inside"));
        let mut index = settings.scan().unwrap().index;
        write(&root, &["inside/b.txt"]);

        let (siblings, bytes) = (index.siblings.clone(), index.heap_bytes());
        let changed = [root.clone(), root.join("insider"), PathBuf::from("inside")];
        assert_eq!(settings.update(&mut index, &changed), Update::default());
        let none: [&Path; 0] = [];
        assert_eq!(settings.update(&mut index, &none), Update::default());
        assert_eq!((&index.siblings, index.heap_bytes()), (&siblings, bytes));
        let mut empty = IndexBuilder::new().finish();
        assert_eq!(
            settings.update(&mut empty, &[root.join("inside")]),
            Update::default()
        );
    }

    #[test]
    fn a_folder_read_again_with_nothing_new_leaves_the_lists_as_they_are() {
        let (_dir, root) = temp_root();
        pad(&root);
        write(&root, &["a/x.txt", "b/y.txt"]);
        let settings = keep_all(&root);
        let mut index = settings.scan().unwrap().index;
        write(&root, &["a/z.txt"]);
        settings.update(&mut index, &[root.join("a")]);
        let (siblings, bytes) = (index.siblings.clone(), index.heap_bytes());

        let update = settings.update(&mut index, &[root.join("a"), root.join("b")]);
        assert_eq!(update.folders_read, 2);
        assert_eq!(
            (update.added, update.removed, update.compacted),
            (0, 0, false)
        );
        assert_eq!((&index.siblings, index.heap_bytes()), (&siblings, bytes));
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

    /// Each list's length and room, but the runs of siblings, which every update builds anew.
    fn lists(index: &Index) -> [(usize, usize); 10] {
        [
            (index.names.len(), index.names.capacity()),
            (
                index.name_starts.groups.len(),
                index.name_starts.groups.capacity(),
            ),
            (
                index.name_starts.offsets.len(),
                index.name_starts.offsets.capacity(),
            ),
            (index.passages.len(), index.passages.capacity()),
            (index.name_letters.len(), index.name_letters.capacity()),
            (
                index.name_digit_starts.len(),
                index.name_digit_starts.capacity(),
            ),
            (index.name_symbols.len(), index.name_symbols.capacity()),
            (index.entry_name.len(), index.entry_name.capacity()),
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
        let capacities = |lists: [(usize, usize); 10]| lists.map(|(_, capacity)| capacity);
        assert_eq!(capacities(lists(&index)), capacities(grown));
    }

    /// A home folder and a volume beside it, each with a plan.
    fn home_and_volume() -> (tempfile::TempDir, PathBuf, tempfile::TempDir, PathBuf) {
        let (home_dir, home) = temp_root();
        pad(&home);
        write(&home, &["docs/plan.md", "docs/notes.txt"]);
        let (volume_dir, volume) = temp_root();
        write(
            &volume,
            &[
                "Photos/2026/trip/plan.md",
                "Photos/2026/trip/IMG_0001.JPG",
                ".Spotlight-V100/store.db",
                "Projects/site/node_modules/react/index.js",
                "top.txt",
            ],
        );
        (home_dir, home, volume_dir, volume)
    }

    fn skip_clutter(home: &Path) -> Settings {
        Settings::new(home, home, Clutter::Skip, Ignore::default(), Access::Full)
    }

    #[test]
    fn a_volume_is_added_ejected_and_added_again_without_reading_home() {
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        let home_only = findable(&index, &home);
        // Never told, so only reading home again would find it.
        write(&home, &["docs/unseen.txt"]);

        let added = settings.add_volume(&mut index, &volume).unwrap();
        assert!(search(&index, "unseen").is_empty());
        fs::remove_file(home.join("docs/unseen.txt")).unwrap();
        // The volume, Photos, 2026, trip, its two files, Projects, site and top.txt; hidden
        // folders and node_modules are clutter there too.
        assert_eq!((added.added, added.removed), (9, 0));
        assert_eq!(index.volumes().collect::<Vec<_>>(), [volume.as_path()]);
        assert_current(&settings, &index, &home, "with the volume");
        let mut plans = [
            home.join("docs/plan.md"),
            volume.join("Photos/2026/trip/plan.md"),
        ];
        plans.sort();
        assert_eq!(search(&index, "plan"), plans);
        let on_volume = findable(&index, &home).len() - home_only.len();
        assert_eq!(on_volume, 8);
        // Depth counts from the volume, as from home.
        let trip = search(&index, "IMG_0001");
        assert_eq!(trip, [volume.join("Photos/2026/trip/IMG_0001.JPG")]);

        let ejected = settings.remove_volume(&mut index, &volume).unwrap();
        assert_eq!(ejected.removed, 9);
        assert_eq!(index.volumes().count(), 0);
        assert_eq!(findable(&index, &home), home_only);
        assert_eq!(search(&index, "plan"), [home.join("docs/plan.md")]);
        assert_current(&settings, &index, &home, "after ejecting it");
        assert!(settings.remove_volume(&mut index, &volume).is_none());

        settings.add_volume(&mut index, &volume).unwrap();
        assert_eq!(findable(&index, &home).len() - home_only.len(), on_volume);
        assert_current(&settings, &index, &home, "plugged in again");
    }

    #[test]
    fn changes_on_a_volume_update_it_in_place() {
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &volume).unwrap();
        write(
            &volume,
            &["Photos/2026/trip/IMG_0002.JPG", "new/plan v2.md"],
        );
        fs::remove_file(volume.join("top.txt")).unwrap();
        write(&home, &["docs/budget.xlsx"]);

        let changed = [
            volume.join("Photos/2026/trip"),
            volume.clone(),
            home.join("docs"),
        ];
        let update = settings.update(&mut index, &changed);
        assert_eq!(update.folders_read, 3);
        assert_eq!((update.added, update.removed), (4, 1));
        assert_current(&settings, &index, &home, "after changes on both");
        assert_eq!(search(&index, "plan v2"), [volume.join("new/plan v2.md")]);
    }

    #[test]
    fn a_volume_survives_saving_compacting_and_opening() {
        let (_h, home, _v, volume) = home_and_volume();
        let (_w, other) = temp_root();
        write(&other, &["a.txt"]);
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &other).unwrap();
        settings.add_volume(&mut index, &volume).unwrap();
        // Ejecting the first volume leaves the second one's root numbered higher than
        // anything a compaction keeps before it.
        settings.remove_volume(&mut index, &other).unwrap();
        fs::remove_dir_all(home.join(PADDING)).unwrap();
        assert!(settings.update(&mut index, &[&home]).compacted);
        assert_eq!(index.volumes().collect::<Vec<_>>(), [volume.as_path()]);
        assert_current(&settings, &index, &home, "after compacting");

        let file = index_file(&home);
        let scan = Scan {
            index,
            unreadable_folders: 0,
        };
        save_index(&file, &scan, &settings, 0).unwrap();
        let opened = open_index(&file, &settings).unwrap().scan.index;
        fs::remove_file(file).unwrap();
        assert!(opened == scan.index);
        assert_current(&settings, &opened, &home, "after opening");
    }

    #[test]
    fn refuses_a_volume_it_cannot_or_need_not_add() {
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &volume).unwrap();
        let before = findable(&index, &home);
        let (_f, loose) = temp_root();
        write(&loose, &["f.txt"]);
        let file = loose.join("f.txt");
        let (_b, backup) = temp_root();
        write(&backup, &["Backups.backupdb/Mac/2026-10-28-101500/a.txt"]);
        let kind = |path: &Path, index: &mut Index| {
            settings
                .add_volume(index, path)
                .map(|_| ())
                .unwrap_err()
                .kind()
        };
        for (path, expected) in [
            (volume.clone(), io::ErrorKind::AlreadyExists),
            (volume.join("Photos"), io::ErrorKind::AlreadyExists),
            (home.clone(), io::ErrorKind::AlreadyExists),
            (home.join("docs"), io::ErrorKind::AlreadyExists),
            (
                home.parent().unwrap().to_path_buf(),
                io::ErrorKind::AlreadyExists,
            ),
            (volume.join("gone"), io::ErrorKind::NotFound),
            (file.clone(), io::ErrorKind::NotADirectory),
            (backup, io::ErrorKind::Unsupported),
        ] {
            assert_eq!(kind(&path, &mut index), expected, "{path:?}");
        }
        assert_eq!(findable(&index, &home), before);
        assert_eq!(index.volumes().count(), 1);
        assert!(settings.remove_volume(&mut index, &home).is_none());
        assert!(settings.remove_volume(&mut index, &file).is_none());

        let limited = Settings::new(
            &home,
            &home,
            Clutter::Skip,
            Ignore::default(),
            Access::Limited,
        );
        let mut index = limited.scan().unwrap().index;
        let (_o, other) = temp_root();
        let refused = limited.add_volume(&mut index, &other).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let mut empty = IndexBuilder::new().finish();
        assert_eq!(kind(&other, &mut empty), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_volume_unplugged_midway_leaves_nothing_behind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        // Once updated, so the runs are found too.
        settings.update(&mut index, &[home.join("docs")]);
        let before = index.clone();
        // Gone once its top folder is read.
        let asked = AtomicUsize::new(0);
        let unplug = || {
            if asked.fetch_add(1, Ordering::Relaxed) == 1 {
                fs::remove_dir_all(&volume).unwrap();
            }
            false
        };
        let failed = settings.add_volume_until(&mut index, &volume, &unplug);
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(asked.load(Ordering::Relaxed) > 1);
        assert!(index == before);

        // Plugged in again, it comes back whole.
        write(&volume, &["Photos/again.jpg"]);
        settings.add_volume(&mut index, &volume).unwrap();
        assert_current(&settings, &index, &home, "plugged in again");
    }

    #[test]
    fn a_volume_ejected_midway_leaves_nothing_behind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        let before = index.clone();
        // Ejected once its top folder is read, so the folders inside are never opened.
        let asked = AtomicUsize::new(0);
        let eject = || asked.fetch_add(1, Ordering::Relaxed) >= 1;
        let stopped = settings.add_volume_until(&mut index, &volume, &eject);
        assert_eq!(stopped.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(index == before);
        // The top folder, one ask for each of Photos and Projects, then one at the end.
        assert_eq!(asked.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_volume_costs_what_scanning_it_alone_costs() {
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        let (home_bytes, home_runs) = (index.heap_bytes(), index.siblings.len());
        let alone = skip_clutter(&volume).scan().unwrap().index;
        let added = settings.add_volume(&mut index, &volume).unwrap();
        assert!(!added.compacted);
        // No room kept for updates, the runs a scan of it makes, and nothing counted toward
        // a compaction.
        assert!(index.heap_bytes() <= home_bytes + alone.heap_bytes());
        assert_eq!(index.siblings.len(), home_runs + alone.siblings.len());
        assert_eq!((index.added, index.removed), (0, 0));
        assert!(!index.children_apart);
        assert_current(&settings, &index, &home, "with the volume");

        // A whole volume larger than home is still no reason to compact.
        let (_b, big) = temp_root();
        pad(&big);
        let added = settings.add_volume(&mut index, &big).unwrap();
        assert!(!added.compacted);
        assert_current(&settings, &index, &home, "with a big volume");
    }

    #[test]
    fn a_volume_beside_a_folder_its_name_starts_with() {
        let (_dir, top) = temp_root();
        let (home, homework) = (top.join("home"), top.join("homework"));
        write(&home, &["notes.txt"]);
        write(&homework, &["essay.txt"]);
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &homework).unwrap();
        assert_eq!(search(&index, "essay"), [homework.join("essay.txt")]);
        let scope = settings.scope(&index);
        write(&homework, &["draft.txt"]);
        assert!(!scope.holds(&homework, &index));
        assert!(scope.holds(&home, &index));
        settings.update(&mut index, &[&homework]);
        assert_current(&settings, &index, &home, "after a change on it");
        assert!(
            settings
                .remove_volume(&mut index, &top.join("home"))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_volume_reached_through_a_link_is_held_by_its_real_path() {
        let (_h, home, _v, volume) = home_and_volume();
        let (_l, links) = temp_root();
        let link = links.join("Photos drive");
        std::os::unix::fs::symlink(&volume, &link).unwrap();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &link).unwrap();
        assert_eq!(index.volumes().collect::<Vec<_>>(), [volume.as_path()]);
        assert!(settings.remove_volume(&mut index, &link).is_none());
        assert!(settings.remove_volume(&mut index, &volume).is_some());
    }

    #[test]
    fn changes_on_two_volumes_at_once() {
        let (_h, home, _v, volume) = home_and_volume();
        let (_o, other) = temp_root();
        write(&other, &["Music/song.mp3"]);
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &volume).unwrap();
        settings.add_volume(&mut index, &other).unwrap();
        write(&volume, &["Photos/2026/trip/IMG_0002.JPG"]);
        fs::remove_dir_all(other.join("Music")).unwrap();
        write(&other, &["Films/clip.mov"]);

        let changed = [volume.join("Photos/2026/trip"), other.join("Music"), other];
        let update = settings.update(&mut index, &changed);
        assert_eq!((update.added, update.removed), (3, 2));
        assert_current(&settings, &index, &home, "after changes on both volumes");
    }

    #[cfg(unix)]
    #[test]
    fn a_volume_read_in_part_still_joins() {
        use std::os::unix::fs::PermissionsExt;
        let (_h, home, _v, volume) = home_and_volume();
        let locked = volume.join("Projects");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        let added = settings.add_volume(&mut index, &volume);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let added = added.unwrap();
        assert_eq!(added.unreadable_folders, 1);
        assert_eq!(search(&index, "Projects"), [locked]);
    }

    #[test]
    fn a_volume_ejected_without_notice_empties_on_the_next_change() {
        let (_h, home, _v, volume) = home_and_volume();
        let settings = skip_clutter(&home);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, &volume).unwrap();
        fs::remove_dir_all(&volume).unwrap();

        let update = settings.update(&mut index, &[volume.join("Photos")]);
        assert_eq!((update.removed, update.unreadable_folders), (8, 0));
        assert_eq!(index.volumes().collect::<Vec<_>>(), [volume.as_path()]);
        assert_eq!(search(&index, "plan"), [home.join("docs/plan.md")]);
        // Only the volume's own entry was left.
        let ejected = settings.remove_volume(&mut index, &volume).unwrap();
        assert_eq!(ejected.removed, 1);
        assert_current(&settings, &index, &home, "after removing it");
    }

    #[test]
    fn added_names_as_long_as_a_disk_allows_keep_their_starts() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/r", NO_PARENT, true);
        builder.push("a", root, false);
        let mut index = builder.finish();
        for round in 0..3 {
            let mut edit = index.edit();
            let mut new = IndexBuilder::new();
            let anchor = new.push("", NO_PARENT, true);
            // 37 names a round, so groups of name starts are split across updates.
            let names: Vec<String> = (0..37)
                .map(|i| char::from_u32(0x65E5 + 37 * round + i).unwrap())
                .map(|letter| std::iter::repeat_n(letter, 255).collect())
                .collect();
            for name in &names {
                new.push(name, anchor, false);
            }
            let first_new = index.len();
            let added = &mut edit.added_parents;
            assert_eq!(index.splice(added, &new.lists(), &[root]), 37);
            for (entry, name) in (first_new as EntryId..).zip(&names) {
                assert_eq!(index.name(entry), name);
            }
            let added = &edit.added_parents;
            index.siblings = (index.siblings).edited(&[], first_new, added, &index.folders);
        }
        assert_sound(&index);
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
            for step in 0..6 {
                let context = format!("seed {seed}, step {step}");
                let mut parents = index.parents();
                let mut edit = index.edit();
                let assert_children = |index: &Index, edit: &Edit, parents: &[EntryId]| {
                    for &folder in &index.folders {
                        let mut children: Vec<EntryId> = index.run_children(edit, folder).collect();
                        children.sort_unstable();
                        let expected: Vec<EntryId> = (0..index.len() as EntryId)
                            .filter(|&entry| parents[entry as usize] == folder)
                            .filter(|entry| !edit.removed.contains(entry))
                            .collect();
                        assert_eq!(children, expected, "{context}");
                    }
                };
                assert_children(&index, &edit, &parents);
                // Removals, each with everything inside it, sometimes inside one already gone.
                for _ in 0..rng.below(4) {
                    let entry = 1 + rng.below(index.len());
                    if entry < index.len() && parents[entry] != NO_PARENT {
                        index.remove(&mut edit, entry as EntryId);
                    }
                }
                assert_children(&index, &edit, &parents);
                for &entry in &edit.removed {
                    parents[entry as usize] = NO_PARENT;
                }
                // Appended children, often several to one folder in a row, and new folders.
                let first_new = index.len();
                let mut live: Vec<EntryId> = (index.folders.iter().copied())
                    .filter(|&folder| folder == 0 || parents[folder as usize] != NO_PARENT)
                    .collect();
                let mut parent = *rng.pick(&live);
                for _ in 0..rng.below(12) {
                    if rng.below(2) == 0 {
                        parent = *rng.pick(&live);
                    }
                    let id = index.len() as EntryId;
                    index.entry_name.push(0);
                    edit.added_parents.push(parent);
                    parents.push(parent);
                    if rng.below(3) == 0 {
                        index.folders.push(id);
                        index.folder_inodes.push(0);
                        live.push(id);
                    }
                }
                let mut removed: Vec<EntryId> = edit.removed.iter().copied().collect();
                removed.sort_unstable();
                let edited = (index.siblings).edited(
                    &removed,
                    first_new,
                    &edit.added_parents,
                    &index.folders,
                );
                assert_eq!(
                    edited,
                    Siblings::from_parents(&parents, &index.folders),
                    "{context}"
                );
                index.siblings = edited;
                index.children_apart = true;
                assert_eq!(index.parents(), parents, "{context}");
                for &folder in &index.folders {
                    if folder != 0 && parents[folder as usize] == NO_PARENT {
                        continue;
                    }
                    let expected: Vec<EntryId> = (0..index.len() as EntryId)
                        .filter(|&entry| parents[entry as usize] == folder)
                        .collect();
                    let children: Vec<EntryId> = index.children(folder).collect();
                    assert_eq!(children, expected, "{context}");
                }
            }
        }
    }
}
