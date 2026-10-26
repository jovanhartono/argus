//! The file-name index: every file and folder under one root, kept as flat lists.

use hashbrown::{DefaultHashBuilder, HashTable};
use std::hash::BuildHasher;
use std::path::PathBuf;

pub type EntryId = u32;

/// The number of a distinct name. Every `README.md` on the disk shares one.
type NameId = u32;

pub(crate) const NO_PARENT: EntryId = EntryId::MAX;

/// The `entry_` lists are read by entry number: entry 7's folder is `entry_parent[7]`.
/// Plain number lists keep memory low and can later be saved to disk as they are.
pub struct Index {
    /// Every distinct name once, back to back.
    names: String,
    /// Where each distinct name starts in `names`, plus one final end position.
    name_starts: Vec<u32>,
    entry_name: Vec<NameId>,
    entry_parent: Vec<EntryId>,
    entry_is_dir: Vec<bool>,
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

    pub fn is_dir(&self, entry: EntryId) -> bool {
        self.entry_is_dir[entry as usize]
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

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_starts.capacity() * size_of::<u32>()
            + self.entry_name.capacity() * size_of::<NameId>()
            + self.entry_parent.capacity() * size_of::<EntryId>()
            + self.entry_is_dir.capacity() * size_of::<bool>()
    }

    fn name_text(&self, name: NameId) -> &str {
        let start = self.name_starts[name as usize] as usize;
        let end = self.name_starts[name as usize + 1] as usize;
        &self.names[start..end]
    }
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
                entry_name: Vec::new(),
                entry_parent: Vec::new(),
                entry_is_dir: Vec::new(),
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
        self.index.entry_is_dir.push(is_dir);
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

    pub(crate) fn finish(self) -> Index {
        let mut index = self.index;
        index.names.shrink_to_fit();
        index.name_starts.shrink_to_fit();
        index.entry_name.shrink_to_fit();
        index.entry_parent.shrink_to_fit();
        index.entry_is_dir.shrink_to_fit();
        index
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
    }
}
