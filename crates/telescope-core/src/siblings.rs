//! Each entry's folder and depth, kept once for every run of entries that share them.

use crate::index::{EntryId, NO_PARENT};
use std::ops::Range;

/// The folder of a root's run, and of runs an update removed.
pub(crate) const NO_FOLDER: u32 = u32::MAX;

/// Entries side by side in one folder. Run `r` holds the entries from `starts[r]` up to the
/// next run's start, all directly inside the folder at place `folders[r]` in `Index::folders`,
/// `depths[r]` folders below the root. A scan adds each folder's children at once, so there is
/// about one run per folder: 3.4 MB for 2.4 million entries in 280,000 folders, each folder's
/// own folder included, instead of 12 MB for a parent and a depth each. The root, and entries
/// an update removed, are in runs of `NO_FOLDER`.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct Siblings {
    pub(crate) starts: Vec<EntryId>,
    pub(crate) folders: Vec<u32>,
    pub(crate) depths: Vec<u8>,
    /// Each folder's own folder, both by place, as its run gives it. Searches with several
    /// words climb from folder to folder, which finding each run would make twice as slow.
    pub(crate) folder_parents: Vec<u32>,
}

impl Siblings {
    /// Runs for `parents`, each entry's parent folder, given every folder's entry number.
    /// Each parent must be an earlier folder.
    pub(crate) fn from_parents(parents: &[EntryId], folders: &[EntryId]) -> Self {
        let mut siblings = Self::default();
        let mut last = (NO_PARENT, NO_FOLDER);
        for (entry, &parent) in parents.iter().enumerate() {
            if parent != last.0 {
                last = (parent, slot_of(folders, parent));
            }
            siblings.push(entry as EntryId, last.1);
        }
        siblings.add_depths(folders, parents.len());
        siblings.shrink_to_fit();
        siblings
    }

    /// These runs after an update that removed the entries in `removed`, sorted, and added
    /// entries from `first_new` on, each inside the folder `added_parents` gives, or a new
    /// root for `NO_PARENT`. Costs a step per run and per change, not per entry.
    pub(crate) fn edited(
        &self,
        removed: &[EntryId],
        first_new: usize,
        added_parents: &[EntryId],
        folders: &[EntryId],
    ) -> Self {
        let mut siblings = Self::default();
        let mut removed = removed.iter().copied().peekable();
        for run in 0..self.len() {
            let entries = self.entries(run, first_new);
            let folder = self.folders[run];
            let mut at = entries.start;
            while let Some(gone) = removed.next_if(|&gone| (gone as usize) < entries.end) {
                if at < gone as usize {
                    siblings.push(at as EntryId, folder);
                }
                siblings.push(gone, NO_FOLDER);
                at = gone as usize + 1;
            }
            if at < entries.end {
                siblings.push(at as EntryId, folder);
            }
        }
        let mut last = (NO_PARENT, NO_FOLDER);
        for (entry, &parent) in (first_new as EntryId..).zip(added_parents) {
            if parent != last.0 {
                last = (parent, slot_of(folders, parent));
            }
            siblings.push(entry, last.1);
        }
        siblings.add_depths(folders, first_new + added_parents.len());
        siblings.shrink_to_fit();
        siblings
    }

    /// Works out each run's depth and each folder's own folder in one pass, as a run's folder
    /// is in an earlier run, for an index of `len` entries whose folders are `folders`.
    pub(crate) fn add_depths(&mut self, folders: &[EntryId], len: usize) {
        let mut folder_depths = vec![0_u8; folders.len()];
        let mut folder_parents = Vec::with_capacity(folders.len());
        let mut depths = Vec::with_capacity(self.len());
        for (run, &folder) in self.folders.iter().enumerate() {
            let depth = match folder {
                NO_FOLDER => 0,
                folder => folder_depths[folder as usize].saturating_add(1),
            };
            depths.push(depth);
            let end = self.entries(run, len).end;
            while folders
                .get(folder_parents.len())
                .is_some_and(|&inside| (inside as usize) < end)
            {
                folder_depths[folder_parents.len()] = depth;
                folder_parents.push(folder);
            }
        }
        self.depths = depths;
        self.folder_parents = folder_parents;
    }

    /// Adds `entry`, the next one, inside the folder at place `folder`. Depths are left for
    /// `add_depths`.
    pub(crate) fn push(&mut self, entry: EntryId, folder: u32) {
        if self.folders.last() != Some(&folder) {
            self.starts.push(entry);
            self.folders.push(folder);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.starts.len()
    }

    /// The run holding `entry`, which must be in the index.
    pub(crate) fn run_of(&self, entry: EntryId) -> usize {
        self.starts.partition_point(|&start| start <= entry) - 1
    }

    /// The entries of run `run` in an index of `len` entries.
    pub(crate) fn entries(&self, run: usize, len: usize) -> Range<usize> {
        let end = self.starts.get(run + 1).map_or(len, |&end| end as usize);
        self.starts[run] as usize..end
    }

    /// Each entry's parent again, for an index of `len` entries whose folders are `folders`.
    pub(crate) fn parents(&self, folders: &[EntryId], len: usize) -> Vec<EntryId> {
        let mut parents = Vec::with_capacity(len);
        for run in 0..self.len() {
            let parent = match self.folders[run] {
                NO_FOLDER => NO_PARENT,
                slot => folders[slot as usize],
            };
            let entries = self.entries(run, len);
            parents.resize(entries.end, parent);
        }
        parents
    }

    /// Finds the runs of entries asked for in order from `entry` on, stepping forward.
    pub(crate) fn cursor(&self, entry: EntryId) -> Cursor<'_> {
        let run = if self.starts.is_empty() {
            0
        } else {
            self.run_of(entry)
        };
        Cursor {
            siblings: self,
            run,
            end: self.end_of(run),
        }
    }

    fn end_of(&self, run: usize) -> EntryId {
        self.starts.get(run + 1).copied().unwrap_or(EntryId::MAX)
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.starts.shrink_to_fit();
        self.folders.shrink_to_fit();
        self.depths.shrink_to_fit();
        self.folder_parents.shrink_to_fit();
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.starts.capacity() * size_of::<EntryId>()
            + self.folders.capacity() * size_of::<u32>()
            + self.depths.capacity()
            + self.folder_parents.capacity() * size_of::<u32>()
    }
}

/// The place of `parent` among `folders`; `NO_FOLDER` for none, as a root has.
fn slot_of(folders: &[EntryId], parent: EntryId) -> u32 {
    match parent {
        NO_PARENT => NO_FOLDER,
        _ => folders
            .binary_search(&parent)
            .expect("a parent is a folder") as u32,
    }
}

pub(crate) struct Cursor<'a> {
    siblings: &'a Siblings,
    run: usize,
    /// Where the current run ends.
    end: EntryId,
}

impl Cursor<'_> {
    /// The run holding `entry`, which is no earlier than the last one asked for.
    pub(crate) fn run(&mut self, entry: EntryId) -> usize {
        while entry >= self.end {
            self.run += 1;
            self.end = self.siblings.end_of(self.run);
        }
        self.run
    }
}
