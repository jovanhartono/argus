//! The file-name index: every file and folder under its roots, kept as flat lists.

use crate::name_starts::NameStarts;
use crate::siblings::{NO_FOLDER, Siblings};
use crate::walk::Kind;
use hashbrown::{DefaultHashBuilder, HashTable};
use rayon::prelude::*;
use std::hash::BuildHasher;
use std::ops::Range;
use std::path::{Path, PathBuf};

pub type EntryId = u32;

/// The number of a distinct name. Every `README.md` on the disk shares one.
pub(crate) type NameId = u32;

pub(crate) const NO_PARENT: EntryId = EntryId::MAX;

/// The `entry_` lists are read by entry number: entry 7's name is `entry_name[7]`.
/// A folder is always numbered before anything inside it, and its children are numbered
/// side by side, as the scan adds them all at once, until an update adds or removes some.
/// Plain number lists keep memory low and can later be saved to disk as they are.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Clone)]
pub struct Index {
    /// Every distinct name once, back to back.
    pub(crate) names: String,
    /// Where each distinct name starts in `names`, plus one final end position.
    pub(crate) name_starts: NameStarts,
    /// Each distinct name's `name_fingerprint`, in three lists.
    pub(crate) name_letters: Vec<u64>,
    pub(crate) name_digit_starts: Vec<u8>,
    pub(crate) name_symbols: Vec<u8>,
    pub(crate) entry_name: Vec<NameId>,
    /// Each entry's folder and depth. A root, and an entry an update removed, which search
    /// skips, have no folder.
    pub(crate) siblings: Siblings,
    /// The folders scanned whole, smallest first, each named by its full path: entry 0, the
    /// folder the scan started from, then each volume added since.
    pub(crate) roots: Vec<EntryId>,
    /// Every folder's entry number, smallest first. Most entries are files, so this is
    /// smaller than a yes/no per entry.
    pub(crate) folders: Vec<EntryId>,
    /// The low half of each folder's inode, by its place in `folders`, so an update notices a
    /// folder replaced by another of the same name.
    pub(crate) folder_inodes: Vec<u32>,
    /// Folders read only on the way to one you brought back, smallest first.
    pub(crate) passages: Vec<EntryId>,
    /// Since the lists were last built: entries removed, the folders among them, and entries
    /// added, whose names are stored again.
    pub(crate) removed: usize,
    pub(crate) removed_folders: usize,
    pub(crate) added: usize,
    /// An update added entries since the scan, so a folder's children may sit apart.
    pub(crate) children_apart: bool,
}

impl Index {
    /// Places in the lists, entries an update removed included.
    pub(crate) fn len(&self) -> usize {
        self.entry_name.len()
    }

    /// Files and folders search can find, the root included.
    pub fn entry_count(&self) -> usize {
        self.len() - self.removed
    }

    pub fn distinct_names(&self) -> usize {
        self.name_starts.len() - 1
    }

    pub fn name(&self, entry: EntryId) -> &str {
        self.name_text(self.entry_name[entry as usize])
    }

    pub fn folder_count(&self) -> usize {
        self.folders.len() - self.removed_folders
    }

    /// Every entry number, removed entries included.
    pub fn ids(&self) -> std::ops::Range<EntryId> {
        0..self.len() as EntryId
    }

    /// Left in place by an update that found it gone; search skips it.
    pub fn is_removed(&self, entry: EntryId) -> bool {
        self.removed > 0 && self.parent(entry) == NO_PARENT && !self.is_root(entry)
    }

    /// A handful at most, so a look along the list beats a binary search.
    pub(crate) fn is_root(&self, entry: EntryId) -> bool {
        self.roots.contains(&entry)
    }

    /// The volumes added to the index, by their real paths, in the order they were added.
    pub fn volumes(&self) -> impl Iterator<Item = &Path> {
        self.roots
            .iter()
            .skip(1)
            .map(|&root| Path::new(self.name(root)))
    }

    pub fn is_dir(&self, entry: EntryId) -> bool {
        self.folders.binary_search(&entry).is_ok()
    }

    pub fn path(&self, entry: EntryId) -> PathBuf {
        let mut chain = vec![entry];
        let mut current = self.parent(entry);
        while current != NO_PARENT {
            chain.push(current);
            current = self.parent(current);
        }
        chain.iter().rev().map(|&e| self.name(e)).collect()
    }

    /// The folder `entry` is in: `NO_PARENT` for the root and for an entry an update removed.
    pub(crate) fn parent(&self, entry: EntryId) -> EntryId {
        debug_assert!(
            (entry as usize) < self.len(),
            "entry {entry} is not in the index"
        );
        match self.siblings.folders[self.siblings.run_of(entry)] {
            NO_FOLDER => NO_PARENT,
            slot => self.folders[slot as usize],
        }
    }

    /// Each entry's parent folder, as `parent` gives it.
    pub(crate) fn parents(&self) -> Vec<EntryId> {
        self.siblings.parents(&self.folders, self.len())
    }

    /// Folders between the entry and its root: 1 for a root's own children.
    pub(crate) fn depth(&self, entry: EntryId) -> u8 {
        self.siblings.depths[self.siblings.run_of(entry)]
    }

    /// The runs of siblings among `entries`, cut to fit: their entries, and the place of their
    /// folder in `folders`.
    pub(crate) fn runs_in(
        &self,
        entries: Range<usize>,
    ) -> impl Iterator<Item = (Range<usize>, u32)> + '_ {
        let siblings = &self.siblings;
        let first = if entries.is_empty() {
            siblings.len()
        } else {
            siblings.run_of(entries.start as EntryId)
        };
        let starts = &siblings.starts[first..];
        let ends = starts.iter().skip(1).map(|&end| end as usize);
        starts
            .iter()
            .zip(ends.chain([self.len()]))
            .zip(&siblings.folders[first..])
            .map(move |((&start, end), &folder)| {
                let within = (start as usize).max(entries.start)..end.min(entries.end);
                (within, folder)
            })
            .take_while(move |(within, _)| within.start < entries.end)
    }

    /// Whether a folder's children may not sit side by side, so finding them reads every
    /// run after the folder.
    fn scattered(&self) -> bool {
        self.children_apart || self.removed > 0
    }

    /// The runs of `folder`'s children; none when it is not a folder.
    fn child_runs(&self, folder: EntryId) -> impl Iterator<Item = Range<usize>> + '_ {
        let slot = self
            .folders
            .binary_search(&folder)
            .ok()
            .map(|slot| slot as u32);
        let scattered = self.scattered();
        let after = if slot.is_some() {
            folder as usize + 1..self.len()
        } else {
            0..0
        };
        self.runs_in(after)
            .skip_while(move |&(_, folder)| Some(folder) != slot)
            .take_while(move |&(_, folder)| scattered || Some(folder) == slot)
            .filter(move |&(_, folder)| Some(folder) == slot)
            .map(|(entries, _)| entries)
    }

    pub(crate) fn child_folders(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        self.child_runs(folder)
            .flat_map(|entries| self.folders_in(entries).iter().copied())
    }

    /// The folders among `entries`.
    pub(crate) fn folders_in(&self, entries: Range<usize>) -> &[EntryId] {
        let from = self
            .folders
            .partition_point(|&f| (f as usize) < entries.start);
        let to = self
            .folders
            .partition_point(|&f| (f as usize) < entries.end);
        &self.folders[from..to]
    }

    /// The root whose real path is `path`.
    pub(crate) fn root_at(&self, path: &Path) -> Option<EntryId> {
        let named = |&&root: &&EntryId| Path::new(self.name(root)) == path;
        self.roots.iter().find(named).copied()
    }

    /// The folder reached from `root` through `names`, one folder at a time.
    pub(crate) fn find_folder<'a>(
        &self,
        root: EntryId,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Option<EntryId> {
        let mut folder = root;
        for name in names {
            folder = self
                .child_folders(folder)
                .find(|&child| self.name(child) == name)?;
        }
        Some(folder)
    }

    pub(crate) fn children(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        self.child_runs(folder)
            .flat_map(|entries| entries.map(|entry| entry as EntryId))
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_starts.heap_bytes()
            + self.name_letters.capacity() * size_of::<u64>()
            + self.name_digit_starts.capacity()
            + self.name_symbols.capacity()
            + self.entry_name.capacity() * size_of::<NameId>()
            + self.siblings.heap_bytes()
            + self.folders.capacity() * size_of::<EntryId>()
            + self.folder_inodes.capacity() * size_of::<u32>()
            + self.passages.capacity() * size_of::<EntryId>()
            + self.roots.capacity() * size_of::<EntryId>()
    }

    pub(crate) fn name_len(&self, name: NameId) -> u32 {
        let name = name as usize;
        (self.name_starts.get(name + 1) - self.name_starts.get(name)) as u32
    }

    pub(crate) fn name_text(&self, name: NameId) -> &str {
        let name = name as usize;
        &self.names[self.name_starts.get(name)..self.name_starts.get(name + 1)]
    }

    /// The letters that start the name or a word in it.
    pub(crate) fn word_starts(&self, name: usize) -> u64 {
        word_starts_of(self.name_letters[name], self.name_digit_starts[name])
    }
}

/// Every bit a letter or digit can set in a fingerprint.
pub(crate) const ALL_LETTERS: u64 = (1 << 36) - 1;
/// Word starts held in a name's fingerprint, above its letters; the rest take a byte of their own.
const STARTS_INSIDE: u32 = 64 - 36;

/// A name's letters, with most of its word starts above them, the starts of the last eight
/// digits, and its symbols. Ten bytes a name, so a million names cost 10 MB. The letter check
/// only knows ASCII, so a name like "café" must always get through.
pub(crate) fn name_fingerprint(text: &str) -> (u64, u8, u8) {
    let (letters, starts, symbols) = fingerprints(text);
    let (letters, symbols) = if text.is_ascii() {
        (letters, symbols)
    } else {
        (ALL_LETTERS, u8::MAX)
    };
    (
        letters | starts << 36,
        (starts >> STARTS_INSIDE) as u8,
        symbols,
    )
}

/// The word starts `name_fingerprint` split in two, put back together.
pub(crate) fn word_starts_of(fingerprint: u64, digit_starts: u8) -> u64 {
    fingerprint >> 36 | u64::from(digit_starts) << STARTS_INSIDE
}

/// The letter fingerprint: one bit for each letter or digit in `text`.
pub(crate) fn letters(text: &str) -> u64 {
    fingerprints(text).0
}

/// One bit for each kind of symbol in `text`.
pub(crate) fn symbols(text: &str) -> u8 {
    fingerprints(text).2
}

/// The letter fingerprint, the letters that start the name or a word in it, like the m, r, f
/// and B in "my-report fooBar.md": every place the fuzzy matcher gives a bonus, and every
/// place a whole word can begin, which is also after a byte like "é" that earns none; and the
/// symbol fingerprint.
pub(crate) fn fingerprints(text: &str) -> (u64, u64, u8) {
    let (mut letters, mut starts, mut symbols, mut before) = (0, 0, 0, 0);
    for &byte in text.as_bytes() {
        letters |= LETTER_BITS[byte as usize];
        starts |= START_BITS[before][byte as usize];
        symbols |= SYMBOL_BITS[byte as usize];
        before = KIND[byte as usize];
    }
    (letters, starts, symbols)
}

pub(crate) fn letter(byte: u8) -> u64 {
    LETTER_BITS[byte as usize]
}

/// The common symbols, then any other ASCII one. A search for "my-app" or "__init__" skips
/// most names on these alone, so they are worth a byte a name.
const SYMBOL_BITS: [u8; 256] = {
    let mut table = [0; 256];
    let mut byte: u8 = 0;
    loop {
        table[byte as usize] = match byte {
            b'.' => 1,
            b'-' => 1 << 1,
            b'_' => 1 << 2,
            b' ' => 1 << 3,
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | 128.. => 0,
            _ => 1 << 4,
        };
        if byte == u8::MAX {
            break;
        }
        byte += 1;
    }
    table
};

/// A table look-up per byte is several times faster than working the bit out, which matters
/// when a saved index of a million names is opened.
const LETTER_BITS: [u64; 256] = {
    let mut table = [0; 256];
    let mut byte: u8 = 0;
    loop {
        table[byte as usize] = match byte.to_ascii_lowercase() {
            b @ b'a'..=b'z' => 1 << (b - b'a'),
            b @ b'0'..=b'9' => 1 << (26 + b - b'0'),
            _ => 0,
        };
        if byte == u8::MAX {
            break;
        }
        byte += 1;
    }
    table
};

/// What a byte is, for the one after it: 0 not a letter or digit, 1 lower case, 2 the rest.
const KIND: [usize; 256] = {
    let mut table = [0; 256];
    let mut byte: u8 = 0;
    loop {
        table[byte as usize] = if byte.is_ascii_lowercase() {
            1
        } else if byte.is_ascii_alphanumeric() {
            2
        } else {
            0
        };
        if byte == u8::MAX {
            break;
        }
        byte += 1;
    }
    table
};

/// The bit a byte adds to the word starts, by the kind of byte before it: a letter or digit
/// after a symbol, and a capital after a lower-case letter.
const START_BITS: [[u64; 256]; 3] = {
    let mut table = [[0; 256]; 3];
    let mut byte: u8 = 0;
    loop {
        if byte.is_ascii_alphanumeric() {
            table[0][byte as usize] = LETTER_BITS[byte as usize];
        }
        if byte.is_ascii_uppercase() {
            table[1][byte as usize] = LETTER_BITS[byte as usize];
        }
        if byte == u8::MAX {
            break;
        }
        byte += 1;
    }
    table
};

#[cfg(test)]
pub(crate) fn word_starts(name: &str) -> u64 {
    fingerprints(name).1
}

pub(crate) struct IndexBuilder {
    index: Index,
    /// Name numbers, found by the hash of the name they point to. Only needed while building.
    lookup: HashTable<NameId>,
    hasher: DefaultHashBuilder,
    /// The last parent's entry and place in `folders`, as a folder's children come in one go.
    last: (EntryId, u32),
}

impl IndexBuilder {
    pub(crate) fn new() -> Self {
        Self {
            index: Index {
                names: String::new(),
                name_starts: NameStarts::from_starts(&[0]).expect("one start"),
                name_letters: Vec::new(),
                name_digit_starts: Vec::new(),
                name_symbols: Vec::new(),
                entry_name: Vec::new(),
                siblings: Siblings::default(),
                roots: Vec::new(),
                folders: Vec::new(),
                folder_inodes: Vec::new(),
                passages: Vec::new(),
                removed: 0,
                removed_folders: 0,
                added: 0,
                children_apart: false,
            },
            lookup: HashTable::new(),
            hasher: DefaultHashBuilder::default(),
            last: (NO_PARENT, NO_FOLDER),
        }
    }

    /// Room for at most `bytes` of names in `count` distinct names, so storing them never
    /// doubles a list.
    pub(crate) fn reserve_names(&mut self, bytes: usize, count: usize) {
        let Self {
            index,
            lookup,
            hasher,
            ..
        } = self;
        index.names.reserve_exact(bytes);
        index.name_starts.reserve_exact(count);
        lookup.reserve(count, |&id| hasher.hash_one(index.name_text(id)));
    }

    pub(crate) fn push(&mut self, name: &str, parent: EntryId, is_dir: bool) -> EntryId {
        let kind = if is_dir { Kind::Folder } else { Kind::File };
        self.push_child(name, parent, kind, 0)
    }

    pub(crate) fn push_child(
        &mut self,
        name: &str,
        parent: EntryId,
        kind: Kind,
        inode: u32,
    ) -> EntryId {
        let entry = self.index.len() as EntryId;
        let name = self.intern(name);
        self.index.entry_name.push(name);
        if parent != self.last.0 {
            let slot = match parent {
                NO_PARENT => NO_FOLDER,
                _ => self
                    .index
                    .folders
                    .binary_search(&parent)
                    .expect("a parent is a folder") as u32,
            };
            self.last = (parent, slot);
        }
        self.index.siblings.push(entry, self.last.1);
        if kind != Kind::File {
            self.index.folders.push(entry);
            self.index.folder_inodes.push(inode);
        }
        if kind == Kind::Passage {
            self.index.passages.push(entry);
        }
        entry
    }

    pub(crate) fn lists(self) -> Index {
        self.index
    }

    pub(crate) fn intern(&mut self, name: &str) -> NameId {
        let hash = self.hasher.hash_one(name);
        if let Some(&id) = self
            .lookup
            .find(hash, |&id| self.index.name_text(id) == name)
        {
            return id;
        }
        let id = self.index.distinct_names() as NameId;
        self.index.names.push_str(name);
        let end = self.index.names.len() as u32;
        assert!(
            self.index.name_starts.push(end),
            "only the first name may be longer than 765 bytes"
        );
        self.lookup.insert_unique(hash, id, |&id| {
            self.hasher.hash_one(self.index.name_text(id))
        });
        id
    }

    /// Fingerprints are added only after the lookup table is freed and the lists trimmed,
    /// to keep the scan's peak low.
    pub(crate) fn finish(self) -> Index {
        drop(self.lookup);
        let mut index = self.index;
        index.names.shrink_to_fit();
        index.name_starts.shrink_to_fit();
        index.entry_name.shrink_to_fit();
        index
            .siblings
            .add_depths(&index.folders, index.entry_name.len());
        index.siblings.shrink_to_fit();
        index.folders.shrink_to_fit();
        index.folder_inodes.shrink_to_fit();
        index.passages.shrink_to_fit();
        let siblings = &index.siblings;
        index.roots = (0..siblings.len())
            .filter(|&run| siblings.folders[run] == NO_FOLDER)
            .flat_map(|run| siblings.entries(run, index.entry_name.len()))
            .map(|entry| entry as EntryId)
            .collect();
        index.add_letters();
        index
    }
}

impl Index {
    /// The lists a saved index keeps, with `runs` as each run's first entry and folder place;
    /// depths and fingerprints are worked out again.
    #[expect(
        clippy::too_many_arguments,
        reason = "one per list a saved index keeps"
    )]
    pub(crate) fn from_lists(
        names: String,
        name_starts: NameStarts,
        entry_name: Vec<NameId>,
        runs: (Vec<EntryId>, Vec<u32>),
        folders: Vec<EntryId>,
        folder_inodes: Vec<u32>,
        passages: Vec<EntryId>,
        roots: Vec<EntryId>,
    ) -> Self {
        let (starts, run_folders) = runs;
        let mut siblings = Siblings {
            starts,
            folders: run_folders,
            ..Siblings::default()
        };
        siblings.add_depths(&folders, entry_name.len());
        let mut index = Self {
            names,
            name_starts,
            name_letters: Vec::new(),
            name_digit_starts: Vec::new(),
            name_symbols: Vec::new(),
            entry_name,
            siblings,
            roots,
            folders,
            folder_inodes,
            passages,
            removed: 0,
            removed_folders: 0,
            added: 0,
            children_apart: false,
        };
        index.count_removed();
        index.add_letters();
        index
    }

    /// Counts the entries and folders an update removed, which no list holds on its own.
    pub(crate) fn count_removed(&mut self) {
        let siblings = &self.siblings;
        let (mut entries, mut folders) = (0, 0);
        for run in 0..siblings.len() {
            if siblings.folders[run] == NO_FOLDER {
                let gone = siblings.entries(run, self.len());
                (entries, folders) = (entries + gone.len(), folders + self.folders_in(gone).len());
            }
        }
        // Every root is a folder without a parent too, as a sound saved index holds.
        self.removed = entries - self.roots.len();
        self.removed_folders = folders - self.roots.len();
    }

    pub(crate) fn add_letters(&mut self) {
        let mut letters = vec![0; self.distinct_names()];
        let mut starts = vec![0; self.distinct_names()];
        let mut symbols = vec![0; self.distinct_names()];
        letters
            .par_iter_mut()
            .zip(&mut starts)
            .zip(&mut symbols)
            .enumerate()
            .for_each(|(name, ((letters, starts), symbols))| {
                (*letters, *starts, *symbols) = name_fingerprint(self.name_text(name as NameId));
            });
        (self.name_letters, self.name_digit_starts) = (letters, starts);
        self.name_symbols = symbols;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rebuilds_paths_and_stores_each_name_once() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let docs = builder.push("docs", root, true);
        let nested = builder.push("README.md", docs, false);
        let top = builder.push("README.md", root, false);
        let index = builder.finish();

        assert_eq!(index.len(), 4);
        assert_eq!(index.distinct_names(), 3);
        assert_eq!(index.path(nested), Path::new("/home/docs/README.md"));
        assert_eq!(index.path(top), Path::new("/home/README.md"));
        assert!(index.is_dir(docs));
        assert!(!index.is_dir(nested));
        assert_eq!(
            index.ids().map(|e| index.depth(e)).collect::<Vec<_>>(),
            [0, 1, 2, 1]
        );
        assert_eq!(index.parents(), [NO_PARENT, 0, 1, 0]);
        assert_eq!(index.siblings.starts, [0, 1, 2, 3]);
    }

    #[test]
    fn a_folder_s_children_share_one_run() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let a = builder.push("a", root, true);
        let b = builder.push("b", root, true);
        builder.push("1", root, false);
        for name in ["x", "y", "z"] {
            builder.push(name, b, false);
        }
        for name in ["x", "y"] {
            builder.push(name, a, false);
        }
        let index = builder.finish();
        assert_eq!(index.siblings.starts, [0, 1, 4, 7]);
        assert_eq!(index.siblings.folders, [NO_FOLDER, 0, 2, 1]);
        assert_eq!(index.siblings.depths, [0, 1, 2, 2]);
        assert_eq!(index.children(a).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(index.children(b).collect::<Vec<_>>(), [4, 5, 6]);
        assert_eq!(index.child_folders(root).collect::<Vec<_>>(), [a, b]);
        assert_eq!(index.children(4).count(), 0);
        let runs: Vec<_> = index.runs_in(2..5).collect();
        assert_eq!(runs, [(2..4, 0), (4..5, 2)]);
        assert_eq!(index.runs_in(3..3).count(), 0);
    }

    #[test]
    fn runs_from_parents_match_the_builder_s() {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        // Each entry's depth and its folder's place, worked out the slow way from `parents`.
        let expected = |parents: &[EntryId], folders: &[EntryId], entry: usize| {
            let (mut depth, mut up) = (0_u8, parents[entry]);
            while up != NO_PARENT {
                (depth, up) = (depth + 1, parents[up as usize]);
            }
            let slot = match parents[entry] {
                NO_PARENT => NO_FOLDER,
                parent => folders.binary_search(&parent).unwrap() as u32,
            };
            (depth, slot)
        };
        for round in 0..300 {
            let mut builder = IndexBuilder::new();
            let mut folders = vec![builder.push("/", NO_PARENT, true)];
            let mut pushed = vec![NO_PARENT];
            // Often several children in a row to one folder, as a scan adds them, and now and
            // then a folder of no parent, as an update's anchors are.
            let mut parent = 0;
            for _ in 0..next(80) {
                if next(3) == 0 {
                    parent = folders[next(folders.len())];
                }
                let (parent, is_dir) = match next(12) {
                    0 => (NO_PARENT, true),
                    _ => (parent, next(3) == 0),
                };
                let entry = builder.push("x", parent, is_dir);
                pushed.push(parent);
                if is_dir {
                    folders.push(entry);
                }
            }
            let index = builder.finish();
            assert_eq!(index.parents(), pushed, "round {round}");
            assert_eq!(
                Siblings::from_parents(&pushed, &index.folders),
                index.siblings
            );
            for entry in index.ids() {
                let (depth, slot) = expected(&pushed, &index.folders, entry as usize);
                assert_eq!(index.depth(entry), depth, "round {round}, entry {entry}");
                assert_eq!(index.parent(entry), pushed[entry as usize]);
                assert_eq!(index.siblings.folders[index.siblings.run_of(entry)], slot);
            }
            for (slot, &folder) in index.folders.iter().enumerate() {
                let (_, parent) = expected(&pushed, &index.folders, folder as usize);
                assert_eq!(index.siblings.folder_parents[slot], parent);
            }

            // Removed entries, sometimes inside a removed folder and sometimes not.
            let mut parents = pushed.clone();
            for _ in 0..next(8) {
                let entry = next(parents.len());
                parents[entry] = NO_PARENT;
            }
            let siblings = Siblings::from_parents(&parents, &index.folders);
            assert_eq!(siblings.parents(&index.folders, index.len()), parents);
            for entry in 0..index.len() {
                let (depth, slot) = expected(&parents, &index.folders, entry);
                let run = siblings.run_of(entry as EntryId);
                assert_eq!((siblings.depths[run], siblings.folders[run]), (depth, slot));
            }

            let (a, b) = (next(index.len() + 1), next(index.len() + 1));
            let (a, b) = (a.min(b), a.max(b));
            let mut at = a;
            for (entries, folder) in index.runs_in(a..b) {
                assert_eq!(entries.start, at, "round {round}, {a}..{b}");
                assert!(!entries.is_empty());
                for entry in entries.clone() {
                    let run = index.siblings.run_of(entry as EntryId);
                    assert_eq!(index.siblings.folders[run], folder);
                }
                at = entries.end;
            }
            assert_eq!(at, b, "round {round}, {a}..{b}");

            let from = next(index.len()) as EntryId;
            let mut cursor = index.siblings.cursor(from);
            for entry in (from..index.len() as EntryId).step_by(1 + next(4)) {
                assert_eq!(cursor.run(entry), index.siblings.run_of(entry));
            }
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is not in the index")]
    fn an_entry_past_the_end_has_no_path() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("a", root, false);
        let index = builder.finish();
        index.path(2);
    }

    #[test]
    fn finds_each_folder_and_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // Wide and deep, so the scan's threads number the folders out of order.
        for a in 0..6 {
            for b in 0..6 {
                for c in 0..4 {
                    let folder = root.join(format!("a{a}/b{b}/c{c}"));
                    std::fs::create_dir_all(&folder).unwrap();
                    std::fs::write(folder.join("f.txt"), "").unwrap();
                }
                std::fs::write(root.join(format!("a{a}/b{b}.txt")), "").unwrap();
            }
        }
        std::fs::create_dir(root.join("a0/empty")).unwrap();
        let ignore = crate::Ignore::default();
        let scan = crate::scan(
            &root,
            &root,
            crate::Clutter::Keep,
            &ignore,
            crate::Access::Full,
        );
        let index = scan.unwrap().index;

        for folder in index.folders.clone() {
            let path = index.path(folder);
            let names: Vec<&str> = path
                .strip_prefix(&root)
                .unwrap()
                .iter()
                .map(|name| name.to_str().unwrap())
                .collect();
            assert_eq!(index.find_folder(0, names), Some(folder), "{path:?}");

            let every = (0..index.len() as EntryId).filter(|&e| index.parent(e) == folder);
            assert_eq!(
                index.children(folder).collect::<Vec<_>>(),
                every.collect::<Vec<_>>()
            );
            let mut listed: Vec<&str> = index.children(folder).map(|e| index.name(e)).collect();
            let mut read: Vec<String> = std::fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            listed.sort_unstable();
            read.sort_unstable();
            assert_eq!(listed, read, "{path:?}");
            let folders: Vec<EntryId> = index
                .children(folder)
                .filter(|&e| index.is_dir(e))
                .collect();
            assert_eq!(index.child_folders(folder).collect::<Vec<_>>(), folders);
        }
        // One run for the root, then one for each folder's children.
        let parents: std::collections::BTreeSet<EntryId> = index
            .parents()
            .into_iter()
            .filter(|&p| p != NO_PARENT)
            .collect();
        assert_eq!(index.siblings.len(), 1 + parents.len());
        assert_eq!(index.find_folder(0, ["a0", "nowhere"]), None);
        assert_eq!(index.find_folder(0, ["a0", "b0.txt"]), None);
    }

    #[test]
    fn fingerprint_ignores_case_and_order() {
        assert_eq!(letters("Plan.md"), letters("dm.nalp"));
        assert_eq!(letters("café") & letters("e"), 0);
    }

    #[test]
    fn fingerprint_gives_each_kind_of_byte_its_bit() {
        assert_eq!(letters(""), 0);
        assert_eq!(letters("aZ"), 1 | 1 << 25);
        assert_eq!(letters("09"), 1 << 26 | 1 << 35);
        assert_eq!(letters(".-_ ~(+\t\u{7f}"), 0);
        assert_eq!(letters("é日本"), 0);
        assert_eq!(name_fingerprint("café").0 & ALL_LETTERS, ALL_LETTERS);
        assert_eq!(symbols("a.b"), 1);
        assert_eq!(symbols("-_ "), 0b1110);
        assert_eq!(symbols("~(+\t\u{7f}"), 1 << 4);
        assert_eq!(symbols("aZ09é日本"), 0);
        assert_eq!(name_fingerprint("a-b").2, 1 << 1);
        assert_eq!(name_fingerprint("café").2, u8::MAX);
    }

    #[test]
    fn symbol_table_matches_the_rule_for_every_byte() {
        for byte in 0..=u8::MAX {
            let expected = match byte {
                b'.' => 1,
                b'-' => 2,
                b'_' => 4,
                b' ' => 8,
                _ if byte.is_ascii_alphanumeric() || !byte.is_ascii() => 0,
                _ => 16,
            };
            assert_eq!(SYMBOL_BITS[byte as usize], expected, "byte {byte:#04x}");
        }
    }

    #[test]
    fn fingerprint_table_matches_the_rule_for_every_byte() {
        let rule = |byte: u8| -> u64 {
            match byte.to_ascii_lowercase() {
                b @ b'a'..=b'z' => 1 << (b - b'a'),
                b @ b'0'..=b'9' => 1 << (26 + b - b'0'),
                _ => 0,
            }
        };
        for byte in 0..=u8::MAX {
            let bits = LETTER_BITS[byte as usize];
            assert_eq!(bits, rule(byte), "byte {byte:#04x}");
            assert_eq!(bits & !ALL_LETTERS, 0, "byte {byte:#04x}");
            assert_eq!(bits, LETTER_BITS[byte.to_ascii_uppercase() as usize]);
        }
    }

    #[test]
    fn a_name_fingerprint_keeps_every_letter_and_word_start() {
        let chars: Vec<char> = ('\0'..='~').chain(['é', '日']).collect();
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        for _ in 0..20_000 {
            let name: String = (0..seed % 12)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    chars[(seed % chars.len() as u64) as usize]
                })
                .collect();
            let (letters, starts, symbols) = fingerprints(&name);
            let (fingerprint, digit_starts, name_symbols) = name_fingerprint(&name);
            let kept = if name.is_ascii() {
                (letters, symbols)
            } else {
                (ALL_LETTERS, u8::MAX)
            };
            assert_eq!((fingerprint & ALL_LETTERS, name_symbols), kept, "{name:?}");
            assert_eq!(
                word_starts_of(fingerprint, digit_starts),
                starts,
                "{name:?}"
            );
        }
        let every = "a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9";
        let (fingerprint, digit_starts, _) = name_fingerprint(every);
        assert_eq!(fingerprint, u64::MAX);
        assert_eq!(digit_starts, u8::MAX);
    }

    #[test]
    fn word_starts_are_the_first_letter_and_those_after_symbols_and_capitals() {
        assert_eq!(word_starts("my-report fooBar.md"), letters("mrfbm"));
        assert_eq!(word_starts(".zshrc"), letters("z"));
        assert_eq!(word_starts("..x"), letters("x"));
        assert_eq!(word_starts("ABC_1"), letters("a1"));
        assert_eq!(word_starts("a.."), letters("a"));
        assert_eq!(word_starts("é38"), letters("3"));
        assert_eq!(word_starts("."), 0);
        assert_eq!(word_starts(""), 0);
    }

    #[test]
    fn word_starts_follow_the_rule_for_every_pair_of_characters() {
        let rule = |name: &str| {
            let name = name.strip_prefix('.').unwrap_or(name).as_bytes();
            let mut bits = match name.first() {
                Some(&byte) if byte.is_ascii_alphanumeric() => letter(byte),
                _ => 0,
            };
            for pair in name.windows(2) {
                let [before, byte] = [pair[0], pair[1]];
                if (!before.is_ascii_alphanumeric() && byte.is_ascii_alphanumeric())
                    || (before.is_ascii_lowercase() && byte.is_ascii_uppercase())
                {
                    bits |= letter(byte);
                }
            }
            bits
        };
        let chars: Vec<char> = (' '..='~').chain(['é', '日']).collect();
        for &a in &chars {
            for &b in &chars {
                for name in [format!("{a}{b}"), format!(".{a}{b}x"), format!("q{a}{b}")] {
                    assert_eq!(word_starts(&name), rule(&name), "{name:?}");
                }
            }
        }
    }
}
