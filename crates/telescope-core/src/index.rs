//! The file-name index: every file and folder under its roots, kept as flat lists.

use crate::update::Runs;
use crate::walk::Kind;
use hashbrown::{DefaultHashBuilder, HashTable};
use rayon::prelude::*;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

pub type EntryId = u32;

/// The number of a distinct name. Every `README.md` on the disk shares one.
pub(crate) type NameId = u32;

pub(crate) const NO_PARENT: EntryId = EntryId::MAX;

/// The `entry_` lists are read by entry number: entry 7's parent folder is `entry_parent[7]`.
/// A folder is always numbered before anything inside it, and its children are numbered
/// side by side, as the scan adds them all at once, until an update adds or removes some.
/// Plain number lists keep memory low and can later be saved to disk as they are.
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Clone)]
pub struct Index {
    /// Every distinct name once, back to back.
    pub(crate) names: String,
    /// Where each distinct name starts in `names`, plus one final end position.
    pub(crate) name_starts: Vec<u32>,
    pub(crate) name_letters: Vec<u64>,
    pub(crate) name_word_starts: Vec<u64>,
    pub(crate) entry_name: Vec<NameId>,
    /// `NO_PARENT` for a root, and for an entry an update removed, which search skips.
    pub(crate) entry_parent: Vec<EntryId>,
    /// Folders between the entry and its root: 1 for a root's own children.
    pub(crate) entry_depth: Vec<u8>,
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
    /// Where each folder's children sit, worked out by the first update.
    pub(crate) runs: Option<Runs>,
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
        self.entry_parent[entry as usize] == NO_PARENT && !self.is_root(entry)
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
        let mut current = entry;
        while self.entry_parent[current as usize] != NO_PARENT {
            current = self.entry_parent[current as usize];
            chain.push(current);
        }
        chain.iter().rev().map(|&e| self.name(e)).collect()
    }

    /// Whether a folder's children may not sit side by side, so finding them reads every
    /// entry after the folder.
    fn scattered(&self) -> bool {
        self.children_apart || self.removed > 0
    }

    pub(crate) fn child_folders(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        let after = self.folders.partition_point(|&entry| entry <= folder);
        let scattered = self.scattered();
        self.folders[after..]
            .iter()
            .copied()
            .skip_while(move |&entry| self.entry_parent[entry as usize] != folder)
            .take_while(move |&entry| scattered || self.entry_parent[entry as usize] == folder)
            .filter(move |&entry| self.entry_parent[entry as usize] == folder)
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
        let scattered = self.scattered();
        (folder + 1..self.len() as EntryId)
            .skip_while(move |&entry| self.entry_parent[entry as usize] != folder)
            .take_while(move |&entry| scattered || self.entry_parent[entry as usize] == folder)
            .filter(move |&entry| self.entry_parent[entry as usize] == folder)
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_starts.capacity() * size_of::<u32>()
            + self.name_letters.capacity() * size_of::<u64>()
            + self.name_word_starts.capacity() * size_of::<u64>()
            + self.entry_name.capacity() * size_of::<NameId>()
            + self.entry_parent.capacity() * size_of::<EntryId>()
            + self.entry_depth.capacity()
            + self.folders.capacity() * size_of::<EntryId>()
            + self.folder_inodes.capacity() * size_of::<u32>()
            + self.passages.capacity() * size_of::<EntryId>()
            + self.roots.capacity() * size_of::<EntryId>()
            + self.runs.as_ref().map_or(0, Runs::heap_bytes)
    }

    pub(crate) fn name_len(&self, name: NameId) -> u32 {
        self.name_starts[name as usize + 1] - self.name_starts[name as usize]
    }

    pub(crate) fn name_text(&self, name: NameId) -> &str {
        let start = self.name_starts[name as usize] as usize;
        let end = self.name_starts[name as usize + 1] as usize;
        &self.names[start..end]
    }
}

/// A name's letter fingerprint and word starts. The letter check only knows ASCII, so a name
/// like "café" must always get through.
pub(crate) fn name_fingerprints(text: &str) -> (u64, u64) {
    let (letters, starts) = fingerprints(text);
    (if text.is_ascii() { letters } else { u64::MAX }, starts)
}

/// The letter fingerprint: one bit for each letter, digit or common symbol in `text`.
pub(crate) fn letters(text: &str) -> u64 {
    fingerprints(text).0
}

/// The letter fingerprint, and the letters that start the name or a word in it, like the
/// m, r, f and B in "my-report fooBar.md": every place the fuzzy matcher gives a bonus, and
/// every place a whole word can begin, which is also after a byte like "é" that earns none.
pub(crate) fn fingerprints(text: &str) -> (u64, u64) {
    let (mut letters, mut starts, mut before) = (0, 0, 0);
    for &byte in text.as_bytes() {
        letters |= LETTER_BITS[byte as usize];
        starts |= START_BITS[before][byte as usize];
        before = KIND[byte as usize];
    }
    (letters, starts)
}

pub(crate) fn letter(byte: u8) -> u64 {
    LETTER_BITS[byte as usize]
}

/// A table look-up per byte is several times faster than working the bit out, which matters
/// when a saved index of a million names is opened.
const LETTER_BITS: [u64; 256] = {
    let mut table = [0; 256];
    let mut byte: u8 = 0;
    loop {
        table[byte as usize] = match byte.to_ascii_lowercase() {
            b @ b'a'..=b'z' => 1 << (b - b'a'),
            b @ b'0'..=b'9' => 1 << (26 + b - b'0'),
            b'.' => 1 << 36,
            b'-' => 1 << 37,
            b'_' => 1 << 38,
            b' ' => 1 << 39,
            128.. => 0,
            _ => 1 << 40,
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
}

impl IndexBuilder {
    pub(crate) fn new() -> Self {
        Self {
            index: Index {
                names: String::new(),
                name_starts: vec![0],
                name_letters: Vec::new(),
                name_word_starts: Vec::new(),
                entry_name: Vec::new(),
                entry_parent: Vec::new(),
                entry_depth: Vec::new(),
                roots: Vec::new(),
                folders: Vec::new(),
                folder_inodes: Vec::new(),
                passages: Vec::new(),
                runs: None,
                removed: 0,
                removed_folders: 0,
                added: 0,
                children_apart: false,
            },
            lookup: HashTable::new(),
            hasher: DefaultHashBuilder::default(),
        }
    }

    /// Room for at most `bytes` of names in `count` distinct names, so storing them never
    /// doubles a list.
    pub(crate) fn reserve_names(&mut self, bytes: usize, count: usize) {
        let Self {
            index,
            lookup,
            hasher,
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
        self.index.entry_parent.push(parent);
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
        self.index.name_starts.push(self.index.names.len() as u32);
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
        index.entry_parent.shrink_to_fit();
        index.folders.shrink_to_fit();
        index.folder_inodes.shrink_to_fit();
        index.passages.shrink_to_fit();
        index.roots = (0..index.len() as EntryId)
            .filter(|&entry| index.entry_parent[entry as usize] == NO_PARENT)
            .collect();
        index.add_depths_and_letters();
        index
    }
}

impl Index {
    /// The lists a saved index keeps; depths and fingerprints are worked out again.
    #[expect(
        clippy::too_many_arguments,
        reason = "one per list a saved index keeps"
    )]
    pub(crate) fn from_lists(
        names: String,
        name_starts: Vec<u32>,
        entry_name: Vec<NameId>,
        entry_parent: Vec<EntryId>,
        folders: Vec<EntryId>,
        folder_inodes: Vec<u32>,
        passages: Vec<EntryId>,
        roots: Vec<EntryId>,
    ) -> Self {
        let mut index = Self {
            names,
            name_starts,
            name_letters: Vec::new(),
            name_word_starts: Vec::new(),
            entry_name,
            entry_parent,
            entry_depth: Vec::new(),
            roots,
            folders,
            folder_inodes,
            passages,
            runs: None,
            removed: 0,
            removed_folders: 0,
            added: 0,
            children_apart: false,
        };
        // Every root is a folder without a parent, as a sound saved index holds.
        index.removed = index
            .entry_parent
            .iter()
            .filter(|&&parent| parent == NO_PARENT)
            .count()
            - index.roots.len();
        index.removed_folders = index
            .folders
            .iter()
            .filter(|&&folder| index.entry_parent[folder as usize] == NO_PARENT)
            .count()
            - index.roots.len();
        index.add_depths_and_letters();
        index
    }

    pub(crate) fn add_depths_and_letters(&mut self) {
        self.entry_depth = Vec::with_capacity(self.len());
        for &parent in &self.entry_parent {
            let depth = match parent {
                NO_PARENT => 0,
                parent => self.entry_depth[parent as usize].saturating_add(1),
            };
            self.entry_depth.push(depth);
        }
        let mut letters = vec![0; self.distinct_names()];
        let mut starts = vec![0; self.distinct_names()];
        letters
            .par_iter_mut()
            .zip(&mut starts)
            .enumerate()
            .for_each(|(name, (letters, starts))| {
                (*letters, *starts) = name_fingerprints(self.name_text(name as NameId));
            });
        (self.name_letters, self.name_word_starts) = (letters, starts);
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
        assert_eq!(index.entry_depth, [0, 1, 2, 1]);
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

            let every =
                (0..index.len() as EntryId).filter(|&e| index.entry_parent[e as usize] == folder);
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
        assert_eq!(letters(".-_ "), 0b1111 << 36);
        assert_eq!(letters("~(+\t\u{7f}"), 1 << 40);
        assert_eq!(letters("é日本"), 0);
    }

    #[test]
    fn fingerprint_table_matches_the_rule_for_every_byte() {
        let rule = |byte: u8| -> u64 {
            match byte.to_ascii_lowercase() {
                b @ b'a'..=b'z' => 1 << (b - b'a'),
                b @ b'0'..=b'9' => 1 << (26 + b - b'0'),
                b'.' => 1 << 36,
                b'-' => 1 << 37,
                b'_' => 1 << 38,
                b' ' => 1 << 39,
                128.. => 0,
                _ => 1 << 40,
            }
        };
        for byte in 0..=u8::MAX {
            let bits = LETTER_BITS[byte as usize];
            assert_eq!(bits, rule(byte), "byte {byte:#04x}");
            assert_eq!(bits >> 41, 0, "byte {byte:#04x}");
            assert_eq!(bits, LETTER_BITS[byte.to_ascii_uppercase() as usize]);
        }
        assert_eq!(LETTER_BITS[0x7F], 1 << 40);
        assert_eq!(LETTER_BITS[0x80], 0);
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
