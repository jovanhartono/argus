//! The saved index: one file that opens in milliseconds, so a restart needs no rescan.
//!
//! Version 3, every number little-endian:
//!
//! | bytes | what                                                       |
//! |-------|------------------------------------------------------------|
//! | 0     | `TL-INDEX`                                                 |
//! | 8     | format version, u32                                        |
//! | 12    | CRC-32 of the whole file, read with these 4 bytes as zeros |
//! | 16    | FSEvents event id the index is current to, u64             |
//! | 24    | folders the scan could not read in full, u32               |
//! | 28    | entries added since the lists were last built, low 31 bits |
//! |       | of a u32; the top bit is set once an update added any      |
//! | 32    | sizes, u32 each: settings bytes, name bytes, distinct      |
//! |       | names, entries, folders, passages, roots                   |
//! | 60    | settings, names, name starts, entry names, entry parents,  |
//! |       | folders, folder inodes, passages, roots; the last seven as |
//! |       | u32 lists, folder inodes as long as folders                |
//!
//! An entry an update removed keeps its place with `NO_PARENT` as its parent, as a root does.
//! Version 1 had no removed or added entries, folder inodes or passages; version 2 had one
//! root, entry 0, and no volumes.

use crate::Ignore;
use crate::index::{EntryId, Index, NO_PARENT, NameId};
use crate::scope::Scope;
use crate::update::Update;
use crate::walk::{Access, Clutter, Rules, Scan, scan_with};
use crc32fast::Hasher;
use rayon::prelude::*;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: [u8; 8] = *b"TL-INDEX";
const VERSION: u32 = 3;
const HEADER: usize = 60;
const CRC: std::ops::Range<usize> = 12..16;
const SIZES: usize = 32;
/// The added count's bits at byte 28; the top bit says an update added children apart.
const ADDED: u32 = u32::MAX >> 1;
/// Lists pass through a buffer this size, so saving or opening never holds a second copy.
const CHUNK: usize = 256 * 1024;

/// Changes with any edit to the files that decide what a scan keeps, so a build with new
/// clutter rules rescans even when its version number is unchanged.
const RULES: u64 = fnv(
    fnv(FNV_START, include_bytes!("walk.rs")),
    include_bytes!("ignore.rs"),
);
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, which a build can work out before the program runs.
const fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash = (hash ^ bytes[at] as u64).wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}

/// What an index is built from: the values `scan` takes. A saved index opens only with the
/// same ones, so turning on `--all`, editing the ignore list, granting Full Disk Access or
/// updating Telescope costs one rescan.
#[derive(Clone, Debug)]
pub struct Settings {
    root: PathBuf,
    rules: Rules,
    key: Vec<u8>,
}

impl Settings {
    pub fn new(root: &Path, home: &Path, clutter: Clutter, ignore: Ignore, access: Access) -> Self {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut key = Vec::new();
        put(&mut key, env!("CARGO_PKG_VERSION").as_bytes());
        key.extend_from_slice(&RULES.to_le_bytes());
        put(&mut key, root.as_os_str().as_encoded_bytes());
        put(&mut key, home.as_os_str().as_encoded_bytes());
        key.push(match access {
            Access::Full => 0,
            Access::Limited => 1,
        });
        match clutter {
            Clutter::Keep => key.push(0),
            Clutter::Skip => {
                key.push(1);
                ignore.encode(&mut key);
            }
        }
        Self {
            root,
            rules: Rules::new(home, clutter, &ignore, access),
            key,
        }
    }

    pub fn scan(&self) -> io::Result<Scan> {
        scan_with(&self.root, &self.rules, &|| false)
    }

    /// The real path of the root, as the index holds it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Brings an index built with these settings up to date with the folders that changed,
    /// as FSEvents reports them: real paths, each a folder whose children changed. A path
    /// outside `root()` and every volume is ignored.
    pub fn update<P: AsRef<Path>>(&self, index: &mut Index, changed: &[P]) -> Update {
        index.update(&self.rules, changed)
    }

    /// Adds a mounted volume to an index built with these settings, scanned by the same
    /// rules, without reading the root or other volumes again. `volume` may be any path to
    /// it; `check_volume` tells which volumes to add. Fails as `scan` does for a volume
    /// that cannot be opened; with `PermissionDenied` without Full Disk Access, as macOS asks
    /// before an app reads an external drive; with `AlreadyExists` for a folder the index
    /// already reads, or one holding it; with `Unsupported` for an older Time Machine disk;
    /// and with `NotFound` for a volume gone before the scan ended, which leaves nothing
    /// behind.
    pub fn add_volume(&self, index: &mut Index, volume: &Path) -> io::Result<Update> {
        self.add_volume_until(index, volume, &|| false)
    }

    /// As `add_volume`, but once `stop` says so the scan ends and fails with `Interrupted`,
    /// leaving nothing behind, so an eject never waits for it.
    pub fn add_volume_until(
        &self,
        index: &mut Index,
        volume: &Path,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<Update> {
        if self.rules.access() == Access::Limited {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let volume = fs::canonicalize(volume)?;
        if crate::volume::holds_backups(&volume) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a Time Machine backup",
            ));
        }
        index.add_volume(&volume, &self.rules, stop)
    }

    /// Takes a volume and everything on it out of the index, as when it is ejected; `volume`
    /// is its real path, as `Index::volumes` lists it. `None` when the index holds no such
    /// volume.
    pub fn remove_volume(&self, index: &mut Index, volume: &Path) -> Option<Update> {
        index.remove_volume(volume)
    }

    /// The folders `index`, built with these settings, holds: the root and its volumes.
    pub fn scope(&self, index: &Index) -> Scope {
        Scope::new(
            self.root.clone(),
            index.volumes().map(Path::to_path_buf).collect(),
            self.rules.clone(),
            self.rules.ignore_folder(),
        )
    }
}

impl PartialEq for Settings {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Settings {}

pub(crate) fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

pub struct Saved {
    pub scan: Scan,
    /// The `FSEvents` event the index was current to when saved, so later changes can be caught up.
    pub event_id: u64,
}

pub enum Opened {
    Saved(Saved),
    Scanned { scan: Scan, why: OpenError },
}

/// Why a saved index could not be used. Each one means a rescan, never a crash.
#[derive(Debug)]
pub enum OpenError {
    Io(io::Error),
    NotAnIndex,
    Version(u32),
    OtherSettings,
    WrongLength,
    Damaged,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::NotAnIndex => f.write_str("not a Telescope index"),
            Self::Version(version) => {
                write!(f, "saved as version {version}, this build reads {VERSION}")
            }
            Self::OtherSettings => f.write_str("built with other settings"),
            Self::WrongLength => f.write_str("wrong length, likely cut short"),
            Self::Damaged => f.write_str("damaged"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<io::Error> for OpenError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::UnexpectedEof => Self::WrongLength,
            _ => Self::Io(error),
        }
    }
}

/// Opens the saved index when it matches the settings, and scans otherwise.
pub fn open_or_scan(file: &Path, settings: &Settings) -> io::Result<Opened> {
    match open_index(file, settings) {
        Ok(saved) => Ok(Opened::Saved(saved)),
        Err(why) => Ok(Opened::Scanned {
            scan: settings.scan()?,
            why,
        }),
    }
}

/// Written beside `file` and renamed over it, so a crash mid-save leaves the old file whole.
/// The temporary name holds the process number and is only ever a new file, so two apps
/// saving at once never share it and an existing file of that name is left alone.
pub fn save_index(file: &Path, scan: &Scan, settings: &Settings, event_id: u64) -> io::Result<()> {
    let temp = file.with_added_extension(format!("{}.tmp", std::process::id()));
    let mut out = BufWriter::with_capacity(CHUNK, File::create_new(&temp)?);
    let saved = write(&mut out, scan, settings, event_id)
        .and_then(|()| out.into_inner().map_err(io::IntoInnerError::into_error))
        .and_then(|file| file.sync_all())
        .and_then(|()| fs::rename(&temp, file));
    if saved.is_err() {
        let _ = fs::remove_file(&temp);
    }
    saved
}

fn write(
    out: &mut (impl Write + Seek),
    scan: &Scan,
    settings: &Settings,
    event_id: u64,
) -> io::Result<()> {
    let index = &scan.index;
    let sizes = [
        settings.key.len(),
        index.names.len(),
        index.distinct_names(),
        index.len(),
        index.folders.len(),
        index.passages.len(),
        index.roots.len(),
    ];
    let mut head = [0; HEADER];
    head[..8].copy_from_slice(&MAGIC);
    head[8..12].copy_from_slice(&VERSION.to_le_bytes());
    head[16..24].copy_from_slice(&event_id.to_le_bytes());
    let unreadable = u32::try_from(scan.unreadable_folders).unwrap_or(u32::MAX);
    head[24..28].copy_from_slice(&unreadable.to_le_bytes());
    let added = u32::try_from(index.added).map_or(ADDED, |added| added.min(ADDED))
        | u32::from(index.children_apart) << 31;
    head[28..SIZES].copy_from_slice(&added.to_le_bytes());
    for (at, size) in head[SIZES..].as_chunks_mut().0.iter_mut().zip(sizes) {
        let size = u32::try_from(size).map_err(|_| io::Error::other("index too large to save"))?;
        *at = size.to_le_bytes();
    }

    let mut crc = Hasher::new();
    crc.update(&head);
    out.write_all(&head)?;
    let mut emit = |bytes: &[u8]| {
        crc.update(bytes);
        out.write_all(bytes)
    };
    emit(&settings.key)?;
    emit(index.names.as_bytes())?;
    let mut buffer = Vec::with_capacity(CHUNK);
    for list in [
        &index.name_starts,
        &index.entry_name,
        &index.entry_parent,
        &index.folders,
        &index.folder_inodes,
        &index.passages,
        &index.roots,
    ] {
        for chunk in list.chunks(CHUNK / 4) {
            buffer.clear();
            buffer.extend(chunk.iter().flat_map(|number| number.to_le_bytes()));
            emit(&buffer)?;
        }
    }
    head[CRC].copy_from_slice(&crc.finalize().to_le_bytes());
    out.seek(SeekFrom::Start(0))?;
    out.write_all(&head)
}

pub fn open_index(file: &Path, settings: &Settings) -> Result<Saved, OpenError> {
    let file = File::open(file)?;
    let length = file.metadata()?.len();
    read(file, length, settings)
}

fn read(mut file: impl Read, length: u64, settings: &Settings) -> Result<Saved, OpenError> {
    let mut head = [0; HEADER];
    file.read_exact(&mut head)?;
    if head[..8] != MAGIC {
        return Err(OpenError::NotAnIndex);
    }
    let number =
        |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    let version = number(8);
    if version != VERSION {
        return Err(OpenError::Version(version));
    }
    let saved_crc = number(CRC.start);
    let event_id = u64::from(number(16)) | u64::from(number(20)) << 32;
    let unreadable_folders = number(24) as usize;
    let added = (number(28) & ADDED) as usize;
    let children_apart = number(28) > ADDED;
    let [
        settings_len,
        names_len,
        distinct,
        entries,
        folders,
        passages,
        roots,
    ] = [0, 4, 8, 12, 16, 20, 24].map(|at| u64::from(number(SIZES + at)));
    // Checked before anything is allocated, so a damaged size never asks for gigabytes.
    let lists = distinct + 1 + 2 * entries + 2 * folders + passages + roots;
    let body = settings_len + names_len + 4 * lists;
    if length != HEADER as u64 + body {
        return Err(OpenError::WrongLength);
    }

    head[CRC].fill(0);
    let mut reader = Reader {
        file,
        crc: Hasher::new(),
        buffer: vec![0; CHUNK],
    };
    reader.crc.update(&head);
    if reader.bytes(settings_len)? != settings.key {
        return Err(OpenError::OtherSettings);
    }
    let names = reader.bytes(names_len)?;
    let name_starts = reader.numbers(distinct + 1)?;
    let entry_name = reader.numbers(entries)?;
    let entry_parent = reader.numbers(entries)?;
    let folder_count = folders;
    let folders = reader.numbers(folder_count)?;
    let folder_inodes = reader.numbers(folder_count)?;
    let passages = reader.numbers(passages)?;
    let roots = reader.numbers(roots)?;
    if reader.crc.finalize() != saved_crc {
        return Err(OpenError::Damaged);
    }
    let names = String::from_utf8(names).map_err(|_| OpenError::Damaged)?;
    let sound = is_sound(
        &names,
        &name_starts,
        &entry_name,
        &entry_parent,
        &folders,
        &passages,
        &roots,
    );
    if !sound {
        return Err(OpenError::Damaged);
    }
    let mut index = Index::from_lists(
        names,
        name_starts,
        entry_name,
        entry_parent,
        folders,
        folder_inodes,
        passages,
        roots,
    );
    index.added = added;
    index.children_apart = children_apart;
    Ok(Saved {
        scan: Scan {
            index,
            unreadable_folders,
        },
        event_id,
    })
}

struct Reader<R> {
    file: R,
    crc: Hasher,
    buffer: Vec<u8>,
}

impl<R: Read> Reader<R> {
    fn bytes(&mut self, len: u64) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; usize::try_from(len).map_err(io::Error::other)?];
        self.file.read_exact(&mut bytes)?;
        self.crc.update(&bytes);
        Ok(bytes)
    }

    fn numbers(&mut self, count: u64) -> io::Result<Vec<u32>> {
        let count = usize::try_from(count).map_err(io::Error::other)?;
        let mut numbers = Vec::with_capacity(count);
        let mut left = count * 4;
        while left > 0 {
            let chunk = &mut self.buffer[..left.min(CHUNK)];
            self.file.read_exact(chunk)?;
            self.crc.update(chunk);
            numbers.extend(chunk.as_chunks().0.iter().map(|&b| u32::from_le_bytes(b)));
            left -= chunk.len();
        }
        Ok(numbers)
    }
}

/// Every number points inside its list, so no search or update can step out of bounds.
pub(crate) fn is_sound(
    names: &str,
    name_starts: &[u32],
    entry_name: &[NameId],
    entry_parent: &[EntryId],
    folders: &[EntryId],
    passages: &[EntryId],
    roots: &[EntryId],
) -> bool {
    let distinct = name_starts.len() - 1;
    let ascending = |list: &[EntryId]| list.windows(2).all(|pair| pair[0] < pair[1]);
    name_starts.first() == Some(&0)
        && name_starts.last().map(|&end| end as usize) == Some(names.len())
        && name_starts.windows(2).all(|pair| pair[0] <= pair[1])
        && name_starts
            .iter()
            .all(|&start| names.is_char_boundary(start as usize))
        && entry_name.iter().all(|&name| (name as usize) < distinct)
        && ascending(folders)
        && folders
            .last()
            .is_none_or(|&last| (last as usize) < entry_parent.len())
        && ascending(passages)
        && passages
            .iter()
            .all(|passage| folders.binary_search(passage).is_ok())
        && (roots.first() == Some(&0) || (entry_parent.is_empty() && roots.is_empty()))
        && ascending(roots)
        && roots.iter().all(|&root| {
            entry_parent.get(root as usize) == Some(&NO_PARENT)
                && folders.binary_search(&root).is_ok()
        })
        && parents_are_folders_in_the_index(entry_parent, folders, roots)
}

/// Each entry's parent is an earlier folder that an update has not removed, so every path
/// walks up to a root and removing a folder reaches everything inside it.
fn parents_are_folders_in_the_index(
    entry_parent: &[EntryId],
    folders: &[EntryId],
    roots: &[EntryId],
) -> bool {
    let mut is_folder = vec![0_u64; entry_parent.len().div_ceil(64)];
    for &folder in folders {
        is_folder[folder as usize / 64] |= 1 << (folder % 64);
    }
    entry_parent
        .par_iter()
        .enumerate()
        .skip(1)
        .all(|(entry, &parent)| {
            let at = parent as usize;
            parent == NO_PARENT
                || (at < entry
                    && is_folder[at / 64] >> (at % 64) & 1 == 1
                    && (entry_parent[at] != NO_PARENT || roots.binary_search(&parent).is_ok()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Searcher;
    use crate::index::IndexBuilder;
    use crate::walk::Kind;

    const HOME: &str = "/nowhere";

    fn settings(root: &Path, clutter: Clutter) -> Settings {
        let home = Path::new(HOME);
        Settings::new(root, home, clutter, Ignore::default(), Access::Full)
    }

    fn keep(root: &Path) -> Settings {
        settings(root, Clutter::Keep)
    }

    fn whole(index: Index) -> Scan {
        Scan {
            index,
            unreadable_folders: 0,
        }
    }

    /// The bytes `save_index` writes, kept in memory.
    fn encode(scan: &Scan, settings: &Settings, event_id: u64) -> Vec<u8> {
        let mut out = io::Cursor::new(Vec::new());
        write(&mut out, scan, settings, event_id).unwrap();
        out.into_inner()
    }

    fn decode(bytes: &[u8], settings: &Settings) -> Result<Saved, OpenError> {
        read(bytes, bytes.len() as u64, settings)
    }

    fn fix_crc(bytes: &mut [u8]) {
        bytes[CRC].fill(0);
        let crc = crc32fast::hash(bytes);
        bytes[CRC].copy_from_slice(&crc.to_le_bytes());
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "docs/README.md",
            "src/main.rs",
            "src/README.md",
            "café/naïve résumé.pdf",
            "node_modules/left-pad/index.js",
        ] {
            let file = dir.path().join(file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "").unwrap();
        }
        fs::create_dir(dir.path().join("empty")).unwrap();
        dir
    }

    /// A small scanned index and its saved bytes.
    fn saved_tree() -> (tempfile::TempDir, Settings, Vec<u8>) {
        let dir = tree();
        let settings = keep(dir.path());
        let bytes = encode(&settings.scan().unwrap(), &settings, 7);
        (dir, settings, bytes)
    }

    /// Lists as given, with no depths or fingerprints, which saving does not read.
    fn raw(names: &str, starts: &[u32], name: &[u32], parent: &[u32], folders: &[u32]) -> Index {
        Index {
            names: names.to_owned(),
            name_starts: starts.to_vec(),
            name_letters: Vec::new(),
            name_word_starts: Vec::new(),
            entry_name: name.to_vec(),
            entry_parent: parent.to_vec(),
            entry_depth: Vec::new(),
            roots: if parent.is_empty() {
                Vec::new()
            } else {
                vec![0]
            },
            folders: folders.to_vec(),
            folder_inodes: vec![0; folders.len()],
            passages: Vec::new(),
            runs: None,
            removed: 0,
            removed_folders: 0,
            added: 0,
            children_apart: false,
        }
    }

    #[test]
    fn opens_what_it_saved() {
        let dir = tree();
        let file = dir.path().join("index");
        let settings = keep(dir.path());
        let mut scanned = settings.scan().unwrap();
        scanned.unreadable_folders = 12;
        save_index(&file, &scanned, &settings, u64::MAX - 1).unwrap();

        let saved = open_index(&file, &settings).unwrap();
        assert_eq!(saved.scan.index, scanned.index);
        assert_eq!(saved.scan.unreadable_folders, 12);
        assert_eq!(saved.event_id, u64::MAX - 1);
        let index = &saved.scan.index;
        let names: Vec<_> = (0..index.len() as EntryId)
            .map(|entry| index.name(entry))
            .collect();
        assert!(names.contains(&"naïve résumé.pdf"));
    }

    #[test]
    fn opens_what_it_saved_for_many_made_up_indexes() {
        let settings = keep(Path::new(HOME));
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        let letters = ["a", "B", "é", ".", " ", "z", "日本", "-"];
        for pass in 0..200 {
            let mut builder = IndexBuilder::new();
            let mut folders = vec![builder.push("/", NO_PARENT, true)];
            for _ in 0..next(300) {
                let name: String = (0..next(6)).map(|_| letters[next(letters.len())]).collect();
                // Now and then a volume, a root of its own.
                if next(40) == 0 {
                    folders.push(builder.push(&format!("/Volumes/{name}"), NO_PARENT, true));
                    continue;
                }
                let parent = folders[next(folders.len())];
                let kind = [Kind::File, Kind::File, Kind::Folder, Kind::Passage][next(4)];
                let is_dir = kind != Kind::File;
                let entry = builder.push_child(&name, parent, kind, next(usize::MAX) as u32);
                if is_dir {
                    folders.push(entry);
                }
            }
            let mut built = builder.finish();
            // Entries an update removed, each with everything inside it, and volumes ejected.
            for _ in 0..next(20) {
                let entry = 1 + next(built.len());
                if entry < built.len() {
                    built.entry_parent[entry] = NO_PARENT;
                }
            }
            if built.roots.len() > 1 && next(2) == 0 {
                built.roots.remove(1 + next(built.roots.len() - 1));
            }
            for entry in 1..built.len() {
                let parent = built.entry_parent[entry];
                if parent != NO_PARENT
                    && !built.roots.contains(&parent)
                    && built.entry_parent[parent as usize] == NO_PARENT
                {
                    built.entry_parent[entry] = NO_PARENT;
                }
            }
            let mut index = Index::from_lists(
                built.names,
                built.name_starts,
                built.entry_name,
                built.entry_parent,
                built.folders,
                built.folder_inodes,
                built.passages,
                built.roots,
            );
            index.added = next(1000);
            let scan = Scan {
                index,
                unreadable_folders: next(1000),
            };
            let event_id = (next(usize::MAX) as u64) << 1;
            let saved = decode(&encode(&scan, &settings, event_id), &settings).unwrap();
            assert!(saved.scan.index == scan.index, "pass {pass}");
            assert_eq!(saved.scan.unreadable_folders, scan.unreadable_folders);
            assert_eq!(saved.event_id, event_id, "pass {pass}");
        }
    }

    #[test]
    fn opens_lists_longer_than_one_chunk() {
        let settings = keep(Path::new(HOME));
        let per_chunk = CHUNK / 4;
        for entries in [per_chunk, per_chunk + 1, 2 * per_chunk + 1] {
            let mut builder = IndexBuilder::new();
            let root = builder.push("/", NO_PARENT, true);
            for entry in 1..entries {
                builder.push(&entry.to_string(), root, entry % 2 == 0);
            }
            let index = builder.finish();
            assert_eq!(index.len(), entries);
            let saved = decode(&encode(&whole(index), &settings, 0), &settings).unwrap();
            let index = &saved.scan.index;
            assert_eq!(index.len(), entries);
            assert_eq!(
                index.name(entries as EntryId - 1),
                (entries - 1).to_string()
            );
            assert!(index.is_dir(2) && !index.is_dir(1));
        }
    }

    #[test]
    fn saves_an_empty_index() {
        let settings = keep(Path::new(HOME));
        let empty = IndexBuilder::new().finish();
        let bytes = encode(&whole(empty), &settings, 0);
        let saved = decode(&bytes, &settings).unwrap();
        assert_eq!(saved.scan.index, IndexBuilder::new().finish());
    }

    #[test]
    fn numbers_are_little_endian_at_fixed_places() {
        let mut index = raw("/ab", &[0, 1, 3], &[0, 1], &[NO_PARENT, 0], &[0, 1]);
        index.folder_inodes = vec![0x0102_0304, 5];
        index.passages = vec![1];
        index.added = 0x0E0F_1011;
        index.children_apart = true;
        let settings = keep(Path::new(HOME));
        let scan = Scan {
            index,
            unreadable_folders: 0x0A0B_0C0D,
        };
        let bytes = encode(&scan, &settings, 0x0102_0304_0506_0708);

        assert_eq!(bytes[..8], *b"TL-INDEX");
        assert_eq!(bytes[8..12], [3, 0, 0, 0]);
        let mut zeroed = bytes.clone();
        zeroed[12..16].fill(0);
        assert_eq!(bytes[12..16], crc32fast::hash(&zeroed).to_le_bytes());
        assert_eq!(bytes[16..24], [8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(bytes[24..28], [0x0D, 0x0C, 0x0B, 0x0A]);
        assert_eq!(bytes[28..32], [0x11, 0x10, 0x0F, 0x8E]);
        let sizes = [settings.key.len() as u32, 3, 2, 2, 2, 1, 1];
        let expected: Vec<u8> = sizes.iter().flat_map(|size| size.to_le_bytes()).collect();
        assert_eq!(bytes[32..60], expected);
        assert_eq!(bytes[60..60 + settings.key.len()], settings.key);
        let lists = &bytes[60 + settings.key.len()..];
        assert_eq!(lists[..3], *b"/ab");
        assert_eq!(lists[3..15], [0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(lists[15..23], [0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(lists[23..31], [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        assert_eq!(lists[31..39], [0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(lists[39..47], [4, 3, 2, 1, 5, 0, 0, 0]);
        assert_eq!(lists[47..51], [1, 0, 0, 0]);
        assert_eq!(lists[51..], [0, 0, 0, 0]);
    }

    #[test]
    fn refuses_a_file_cut_short_at_any_length() {
        let (_dir, settings, bytes) = saved_tree();
        for length in 0..bytes.len() {
            let opened = decode(&bytes[..length], &settings);
            assert!(
                matches!(opened, Err(OpenError::WrongLength)),
                "length {length}"
            );
        }
        let longer = [bytes.as_slice(), &[0]].concat();
        assert!(matches!(
            decode(&longer, &settings),
            Err(OpenError::WrongLength)
        ));
    }

    #[test]
    fn refuses_any_flipped_bit_for_the_reason_of_its_place() {
        let (_dir, settings, bytes) = saved_tree();
        let settings_end = HEADER + settings.key.len();
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1 << bit;
                let opened = decode(&damaged, &settings);
                let expected = match at {
                    0..8 => matches!(opened, Err(OpenError::NotAnIndex)),
                    8..12 => matches!(opened, Err(OpenError::Version(_))),
                    12..SIZES => matches!(opened, Err(OpenError::Damaged)),
                    SIZES..HEADER => matches!(opened, Err(OpenError::WrongLength)),
                    _ if at < settings_end => matches!(opened, Err(OpenError::OtherSettings)),
                    _ => matches!(opened, Err(OpenError::Damaged)),
                };
                assert!(expected, "byte {at} bit {bit}: {:?}", opened.err());
            }
        }
        assert!(decode(&bytes, &settings).is_ok());
    }

    #[test]
    fn refuses_huge_sizes_before_reading_them() {
        let (_dir, settings, bytes) = saved_tree();
        for at in [SIZES + 12, SIZES + 4, SIZES] {
            let mut huge = bytes.clone();
            huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            fix_crc(&mut huge);
            assert!(matches!(
                decode(&huge, &settings),
                Err(OpenError::WrongLength)
            ));
        }
        let mut all = bytes.clone();
        all[SIZES..HEADER].fill(0xFF);
        assert!(matches!(
            decode(&all, &settings),
            Err(OpenError::WrongLength)
        ));
    }

    #[test]
    fn refuses_other_versions_and_other_files() {
        let (dir, settings, bytes) = saved_tree();
        for version in [
            0,
            1,
            2,
            4,
            u32::MAX,
            u32::from_be_bytes(VERSION.to_le_bytes()),
        ] {
            let mut old = bytes.clone();
            old[8..12].copy_from_slice(&version.to_le_bytes());
            let opened = decode(&old, &settings);
            assert!(matches!(opened, Err(OpenError::Version(v)) if v == version));
        }
        let mut other = bytes.clone();
        other[..8].copy_from_slice(b"bplist00");
        assert!(matches!(
            decode(&other, &settings),
            Err(OpenError::NotAnIndex)
        ));
        let opened = open_index(&dir.path().join("missing"), &settings);
        assert!(matches!(opened, Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn opens_only_with_the_settings_it_was_built_with() {
        let (dir, _, bytes) = saved_tree();
        let root = dir.path();
        let home = Path::new(HOME);
        let none = Ignore::default;
        let (ignore, _) = Ignore::parse("docs\n", root, Access::Full);
        let other = [
            Settings::new(root, home, Clutter::Skip, none(), Access::Full),
            Settings::new(&root.join("src"), home, Clutter::Keep, none(), Access::Full),
            Settings::new(root, root, Clutter::Keep, none(), Access::Full),
            Settings::new(root, home, Clutter::Keep, none(), Access::Limited),
        ];
        for settings in other {
            let opened = decode(&bytes, &settings);
            assert!(matches!(opened, Err(OpenError::OtherSettings)));
        }
        // --all leaves the ignore list out, so editing it changes nothing.
        let edited = Settings::new(root, home, Clutter::Keep, ignore, Access::Full);
        assert!(decode(&bytes, &edited).is_ok());
        let dotted = root.join("src").join("..");
        assert!(decode(&bytes, &keep(&dotted)).is_ok());
    }

    #[test]
    fn a_list_saved_by_another_release_is_scanned_again() {
        let dir = tree();
        let root = dir.path();
        let current = keep(root);
        let version = env!("CARGO_PKG_VERSION");
        assert!(current.key[8..].starts_with(version.as_bytes()));
        let mut older = current.clone();
        older.key.clear();
        put(&mut older.key, b"0.1.0");
        older
            .key
            .extend_from_slice(&current.key[8 + version.len()..]);
        assert_ne!(older, current);

        let scan = current.scan().unwrap();
        let bytes = encode(&scan, &older, 1);
        assert!(matches!(
            decode(&bytes, &current),
            Err(OpenError::OtherSettings)
        ));

        let saves = tempfile::tempdir().unwrap();
        let file = saves.path().join("index");
        save_index(&file, &scan, &older, 1).unwrap();
        let Opened::Scanned { scan: fresh, why } = open_or_scan(&file, &current).unwrap() else {
            panic!("opened a list an older release saved");
        };
        assert!(matches!(why, OpenError::OtherSettings));
        assert_eq!(fresh.index.len(), scan.index.len());
    }

    #[test]
    fn every_ignore_rule_counts_but_comments_do_not() {
        let root = Path::new(HOME);
        let rules = |text: &str| {
            let (ignore, problems) = Ignore::parse(text, root, Access::Full);
            assert!(problems.is_empty(), "{text:?}");
            Settings::new(root, root, Clutter::Skip, ignore, Access::Full)
        };
        let differ = [
            ("docs", "!docs"),
            ("~/docs", "docs"),
            ("~/docs", "!~/docs"),
            ("!docs", "!~/docs"),
            ("ab", "a\nb"),
            ("docs", ""),
        ];
        for (a, b) in differ {
            assert_ne!(rules(a), rules(b), "{a:?} and {b:?}");
        }
        let same = [
            ("docs\n~/x", "# mine\n\ndocs\n  ~/x/\n# end"),
            ("!~/x", "! ~/x/"),
        ];
        for (a, b) in same {
            assert_eq!(rules(a), rules(b), "{a:?} and {b:?}");
        }
    }

    #[test]
    fn rules_hash_is_fnv_1a() {
        assert_eq!(fnv(FNV_START, b""), FNV_START);
        assert_eq!(fnv(FNV_START, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv(fnv(FNV_START, b"fo"), b"o"), fnv(FNV_START, b"foo"));
        let key = keep(Path::new(HOME)).key;
        assert!(key.windows(8).any(|bytes| bytes == RULES.to_le_bytes()));
    }

    #[test]
    fn refuses_lists_that_point_outside_themselves() {
        let settings = keep(Path::new(HOME));
        let with_passages = |passages: &[u32]| {
            let mut index = raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0, 1]);
            index.passages = passages.to_vec();
            index
        };
        // The root, a volume and a file on it.
        let with_roots = |parents: &[u32], folders: &[u32], roots: &[u32]| {
            let mut index = raw("/abc", &[0, 1, 2, 4], &[0, 1, 2], parents, folders);
            index.roots = roots.to_vec();
            index
        };
        let volume = [NO_PARENT, NO_PARENT, 1];
        let sound = [
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            // Removed by an update.
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, NO_PARENT], &[0]),
            with_passages(&[1]),
            with_roots(&volume, &[0, 1], &[0, 1]),
            // An ejected volume, everything on it removed.
            with_roots(&[NO_PARENT; 3], &[0, 1], &[0]),
            // Two roots of one name, which no update makes: odd, yet nothing points outside
            // a list, and a path finds the first.
            {
                let mut twins = with_roots(&volume, &[0, 1], &[0, 1]);
                twins.entry_name[1] = twins.entry_name[0];
                assert_eq!(twins.root_at(Path::new(twins.name(1))), Some(0));
                twins
            },
        ];
        let unsound_roots = [
            with_roots(&volume, &[0, 1], &[1]),
            with_roots(&volume, &[0, 1], &[]),
            with_roots(&volume, &[0, 1], &[0, 1, 1]),
            with_roots(&volume, &[0, 1], &[0, 3]),
            with_roots(&volume, &[0, 1], &[0, NO_PARENT]),
            with_roots(&volume, &[0], &[0, 1]),
            with_roots(&[NO_PARENT, 0, 1], &[0, 1], &[0, 1]),
            // A file left on an ejected volume.
            with_roots(&volume, &[0, 1], &[0]),
            {
                let mut empty = raw("", &[0], &[], &[], &[]);
                empty.roots = vec![0];
                empty
            },
        ];
        let unsound = [
            raw("/aé", &[0, 1, 3], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[1, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 2, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 3, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 2], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[0, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 1], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0, 0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[2]),
            raw("", &[0], &[], &[], &[0]),
            with_passages(&[1, 1]),
            with_passages(&[2]),
            with_passages(&[NO_PARENT]),
            // A file as a parent, and an entry left inside a removed folder.
            raw("/abc", &[0, 1, 2, 4], &[0, 1, 2], &[NO_PARENT, 0, 1], &[0]),
            raw(
                "/abc",
                &[0, 1, 2, 4],
                &[0, 1, 2],
                &[NO_PARENT, NO_PARENT, 1],
                &[0, 1],
            ),
        ];
        for index in sound {
            assert!(decode(&encode(&whole(index), &settings, 0), &settings).is_ok());
        }
        for (i, index) in unsound.into_iter().chain(unsound_roots).enumerate() {
            let opened = decode(&encode(&whole(index), &settings, 0), &settings);
            assert!(matches!(opened, Err(OpenError::Damaged)), "case {i}");
        }
    }

    #[test]
    fn refuses_names_that_are_not_utf_8() {
        let settings = keep(Path::new(HOME));
        let index = raw("/ab", &[0, 1, 3], &[0, 1], &[NO_PARENT, 0], &[0]);
        let mut bytes = encode(&whole(index), &settings, 0);
        bytes[HEADER + settings.key.len() + 1] = 0xFF;
        fix_crc(&mut bytes);
        assert!(matches!(decode(&bytes, &settings), Err(OpenError::Damaged)));
    }

    /// Lists damaged so the CRC still holds, as a bug in saving would leave them: either
    /// refused, or safe to show, search and update.
    #[test]
    fn damaged_lists_with_a_good_crc_never_panic() {
        let (dir, settings, bytes) = saved_tree();
        let root = fs::canonicalize(dir.path()).unwrap();
        let changed = [root.clone(), root.join("src"), root.join("docs/README.md")];
        let names_at = HEADER + settings.key.len();
        let names_len = u32::from_le_bytes(bytes[SIZES + 4..SIZES + 8].try_into().unwrap());
        let lists_at = names_at + names_len as usize;
        let entries = u32::from_le_bytes(bytes[SIZES + 12..SIZES + 16].try_into().unwrap());
        let searcher = Searcher::with_threads(1);
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        let mut opened = 0;
        for pass in 0..3000 {
            let mut damaged = bytes.clone();
            for _ in 0..=next(3) {
                if next(4) == 0 {
                    damaged[names_at + next(names_len as usize)] ^= 1 << next(8);
                } else {
                    let at = lists_at + 4 * next((bytes.len() - lists_at) / 4);
                    let number = match next(8) {
                        0 => NO_PARENT,
                        _ => next(entries as usize + 2) as u32,
                    };
                    damaged[at..at + 4].copy_from_slice(&number.to_le_bytes());
                }
            }
            fix_crc(&mut damaged);
            let Ok(saved) = decode(&damaged, &settings) else {
                continue;
            };
            opened += 1;
            let mut index = saved.scan.index;
            for entry in 0..index.len() as EntryId {
                let _ = (index.path(entry), index.name(entry), index.is_dir(entry));
            }
            let apps = crate::Apps::default();
            for query in ["a", "re", "md src", "é"] {
                let hits = searcher.search(&index, &apps, query, searcher.ticket(), 10);
                assert!(hits.is_some(), "pass {pass}");
            }
            settings.update(&mut index, &changed);
            let hits = searcher.search(&index, &apps, "md src", searcher.ticket(), 10);
            assert!(hits.is_some(), "pass {pass}");
        }
        assert!(opened > 100, "only {opened} damaged indexes opened");
    }

    #[test]
    fn scans_when_the_saved_index_cannot_be_used() {
        let dir = tree();
        let root = &fs::canonicalize(dir.path()).unwrap();
        let file = root.join("index");
        let all = keep(root);
        let opened = open_or_scan(&file, &all).unwrap();
        let Opened::Scanned { scan, why } = opened else {
            panic!("opened a file that is not there");
        };
        assert!(matches!(why, OpenError::Io(e) if e.kind() == io::ErrorKind::NotFound));

        save_index(&file, &scan, &all, 3).unwrap();
        let Opened::Saved(saved) = open_or_scan(&file, &all).unwrap() else {
            panic!("rescanned a good file");
        };
        assert_eq!(saved.event_id, 3);
        assert_eq!(saved.scan.index, scan.index);

        let skip = settings(root, Clutter::Skip);
        let Opened::Scanned { scan, why } = open_or_scan(&file, &skip).unwrap() else {
            panic!("opened an index built with --all without it");
        };
        assert!(matches!(why, OpenError::OtherSettings));
        let paths = |index: &Index| {
            let mut paths: Vec<_> = (0..index.len() as EntryId)
                .map(|entry| index.path(entry))
                .collect();
            paths.sort();
            paths
        };
        let rescanned = paths(&scan.index);
        assert_eq!(rescanned, paths(&skip.scan().unwrap().index));
        assert!(rescanned.contains(&root.join("src").join("main.rs")));
        assert!(!rescanned.contains(&root.join("node_modules")));
        assert!(paths(&saved.scan.index).contains(&root.join("node_modules")));

        fs::write(&file, "garbage").unwrap();
        let opened = open_or_scan(&file, &all).unwrap();
        assert!(matches!(
            opened,
            Opened::Scanned {
                why: OpenError::WrongLength,
                ..
            }
        ));
        assert!(open_or_scan(&file, &keep(&root.join("missing"))).is_err());
    }

    #[test]
    fn replaces_the_old_file_and_leaves_no_temporary_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let scan = whole(raw("/", &[0, 1], &[0], &[NO_PARENT], &[0]));
        let settings = keep(dir.path());
        save_index(&file, &scan, &settings, 1).unwrap();
        save_index(&file, &scan, &settings, 2).unwrap();
        assert_eq!(open_index(&file, &settings).unwrap().event_id, 2);
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);

        let nowhere = dir.path().join("missing").join("index");
        assert!(save_index(&nowhere, &scan, &settings, 1).is_err());
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn a_save_that_fails_late_removes_its_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        fs::create_dir(&file).unwrap();
        fs::write(file.join("inside"), "mine").unwrap();
        let scan = whole(raw("/", &[0, 1], &[0], &[NO_PARENT], &[0]));

        assert!(save_index(&file, &scan, &keep(dir.path()), 1).is_err());
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(fs::read_to_string(file.join("inside")).unwrap(), "mine");
    }

    #[test]
    fn never_writes_over_a_file_with_the_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let temp = file.with_added_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temp, "mine").unwrap();
        let scan = whole(raw("/", &[0, 1], &[0], &[NO_PARENT], &[0]));

        let saved = save_index(&file, &scan, &keep(dir.path()), 1);
        assert_eq!(saved.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&temp).unwrap(), "mine");
        assert!(!file.exists());
    }
}
