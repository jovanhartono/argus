//! The file-name index: every file and folder under one root, kept as flat lists.

use hashbrown::{DefaultHashBuilder, HashTable};
use rayon::prelude::*;
use std::hash::BuildHasher;
use std::path::PathBuf;

pub type EntryId = u32;

/// The number of a distinct name. Every `README.md` on the disk shares one.
pub(crate) type NameId = u32;

pub(crate) const NO_PARENT: EntryId = EntryId::MAX;

/// The `entry_` lists are read by entry number: entry 7's parent folder is `entry_parent[7]`.
/// A folder is always numbered before anything inside it.
/// Plain number lists keep memory low and can later be saved to disk as they are.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct Index {
    /// Every distinct name once, back to back.
    pub(crate) names: String,
    /// Where each distinct name starts in `names`, plus one final end position.
    pub(crate) name_starts: Vec<u32>,
    pub(crate) name_letters: Vec<u64>,
    pub(crate) entry_name: Vec<NameId>,
    pub(crate) entry_parent: Vec<EntryId>,
    /// Folders between the entry and the root: 1 for the root's own children.
    pub(crate) entry_depth: Vec<u8>,
    /// Every folder's entry number, smallest first. Most entries are files, so this is
    /// smaller than a yes/no per entry.
    pub(crate) folders: Vec<EntryId>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.entry_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entry_name.is_empty()
    }

    pub fn distinct_names(&self) -> usize {
        self.name_starts.len() - 1
    }

    pub fn name(&self, entry: EntryId) -> &str {
        self.name_text(self.entry_name[entry as usize])
    }

    pub fn folder_count(&self) -> usize {
        self.folders.len()
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

    /// Folders are numbered after their parent, so the search starts there.
    pub(crate) fn child_folders(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        let after = self.folders.partition_point(|&entry| entry <= folder);
        self.folders[after..]
            .iter()
            .copied()
            .filter(move |&entry| self.entry_parent[entry as usize] == folder)
    }

    /// The folder reached from the root through `names`, one folder at a time.
    pub(crate) fn find_folder<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Option<EntryId> {
        let mut folder = 0;
        for name in names {
            folder = self
                .child_folders(folder)
                .find(|&child| self.name(child) == name)?;
        }
        Some(folder)
    }

    /// Everything listed directly inside `folder`.
    pub(crate) fn children(&self, folder: EntryId) -> impl Iterator<Item = EntryId> + '_ {
        (folder + 1..self.len() as EntryId)
            .filter(move |&entry| self.entry_parent[entry as usize] == folder)
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_starts.capacity() * size_of::<u32>()
            + self.name_letters.capacity() * size_of::<u64>()
            + self.entry_name.capacity() * size_of::<NameId>()
            + self.entry_parent.capacity() * size_of::<EntryId>()
            + self.entry_depth.capacity()
            + self.folders.capacity() * size_of::<EntryId>()
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

/// The letter fingerprint: one bit for each letter, digit or common symbol in `text`.
pub(crate) fn letters(text: &str) -> u64 {
    text.bytes()
        .fold(0, |bits, byte| bits | LETTER_BITS[byte as usize])
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
                entry_name: Vec::new(),
                entry_parent: Vec::new(),
                entry_depth: Vec::new(),
                folders: Vec::new(),
            },
            lookup: HashTable::new(),
            hasher: DefaultHashBuilder::default(),
        }
    }

    pub(crate) fn push(&mut self, name: &str, parent: EntryId, is_dir: bool) -> EntryId {
        let entry = self.index.len() as EntryId;
        let name = self.intern(name);
        self.index.entry_name.push(name);
        self.index.entry_parent.push(parent);
        if is_dir {
            self.index.folders.push(entry);
        }
        entry
    }

    fn intern(&mut self, name: &str) -> NameId {
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
        index.add_depths_and_letters();
        index
    }
}

impl Index {
    /// The lists a saved index keeps; depths and fingerprints are worked out again.
    pub(crate) fn from_lists(
        names: String,
        name_starts: Vec<u32>,
        entry_name: Vec<NameId>,
        entry_parent: Vec<EntryId>,
        folders: Vec<EntryId>,
    ) -> Self {
        let mut index = Self {
            names,
            name_starts,
            name_letters: Vec::new(),
            entry_name,
            entry_parent,
            entry_depth: Vec::new(),
            folders,
        };
        index.add_depths_and_letters();
        index
    }

    fn add_depths_and_letters(&mut self) {
        self.entry_depth = Vec::with_capacity(self.len());
        for &parent in &self.entry_parent {
            let depth = match parent {
                NO_PARENT => 0,
                parent => self.entry_depth[parent as usize].saturating_add(1),
            };
            self.entry_depth.push(depth);
        }
        self.name_letters = (0..self.distinct_names())
            .into_par_iter()
            .map(|name| {
                let text = self.name_text(name as NameId);
                // The check only knows ASCII, so a name like "café" must always get through.
                if text.is_ascii() {
                    letters(text)
                } else {
                    u64::MAX
                }
            })
            .collect();
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
}
