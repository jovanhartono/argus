//! The saved index: one file that opens in milliseconds, so a restart needs no rescan.
//!
//! Version 4, every number little-endian:
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
//! |       | names, entries, runs, folders, passages, roots             |
//! | 64    | settings, names, name starts, entry names, run starts, run |
//! |       | folders, folders, folder inodes, passages, roots; the last |
//! |       | eight as u32 lists, run folders as long as run starts and  |
//! |       | folder inodes as long as folders                           |
//!
//! A run is entries side by side in one folder, given by its place among the folders; the
//! runs of roots, and of entries an update removed, have `NO_FOLDER`. Version 3 kept a parent
//! for every entry; version 2 also had one root, entry 0, and no volumes; version 1 also had
//! no removed or added entries, folder inodes or passages.

use crate::Ignore;
use crate::index::{EntryId, Index, NameId};
use crate::name_starts::NameStarts;
use crate::scope::Scope;
use crate::siblings::NO_FOLDER;
use crate::update::{Changes, Update};
use crate::walk::{Access, Clutter, Rules, Scan, scan_with};
use crc32fast::Hasher;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAGIC: [u8; 8] = *b"TL-INDEX";
const VERSION: u32 = 4;
const HEADER: usize = 64;
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
/// updating Argus costs one rescan.
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

    /// Ties a saved index to one FSEvents log, as macOS names it: once the log is wiped, the
    /// event id an index was saved at means nothing, so the index no longer opens.
    pub fn with_journal(mut self, journal: &[u8]) -> Self {
        put(&mut self.key, journal);
        self
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

    /// As `update`, in two steps, so searches can go on while the folders are read: this reads
    /// them against `index`, leaving it as it is, and `Changes::apply` brings it up to date.
    pub fn read_update<P: AsRef<Path>>(&self, index: &Index, changed: &[P]) -> Changes {
        index.read_update(&self.rules, changed)
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

    /// As `remove_volume`, in two steps, as `read_update`.
    pub fn read_volume_removal(&self, index: &Index, volume: &Path) -> Option<Changes> {
        index.read_volume_removal(volume)
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
            Self::NotAnIndex => f.write_str("not an Argus index"),
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
pub fn save_index(
    file: &Path,
    index: &Index,
    unreadable_folders: usize,
    settings: &Settings,
    event_id: u64,
) -> io::Result<()> {
    let temp = file.with_added_extension(format!("{}.tmp", std::process::id()));
    let mut out = BufWriter::with_capacity(CHUNK, File::create_new(&temp)?);
    let saved = write(&mut out, index, unreadable_folders, settings, event_id)
        .and_then(|()| out.into_inner().map_err(io::IntoInnerError::into_error))
        .and_then(|file| file.sync_all())
        .and_then(|()| fs::rename(&temp, file));
    if saved.is_err() {
        let _ = fs::remove_file(&temp);
    }
    saved
}

/// Removes what saves cut short left beside `file`, as when the app quits mid-save, once
/// `age` old: a save still running in another copy of the app is younger.
pub fn remove_old_temps(file: &Path, age: Duration) {
    let (Some(folder), Some(name)) = (file.parent(), file.file_name()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    let name = name.as_encoded_bytes();
    for entry in entries.flatten() {
        let left = entry.file_name();
        let process = left
            .as_encoded_bytes()
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(b"."))
            .and_then(|rest| rest.strip_suffix(b".tmp"));
        let temp = process.is_some_and(|id| !id.is_empty() && id.iter().all(u8::is_ascii_digit));
        let old = || {
            let modified = entry.metadata().and_then(|meta| meta.modified());
            modified.is_ok_and(|at| at.elapsed().is_ok_and(|since| since >= age))
        };
        if temp && old() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn write(
    out: &mut (impl Write + Seek),
    index: &Index,
    unreadable_folders: usize,
    settings: &Settings,
    event_id: u64,
) -> io::Result<()> {
    let sizes = [
        settings.key.len(),
        index.names.len(),
        index.distinct_names(),
        index.len(),
        index.siblings.len(),
        index.folders.len(),
        index.passages.len(),
        index.roots.len(),
    ];
    let mut head = [0; HEADER];
    head[..8].copy_from_slice(&MAGIC);
    head[8..12].copy_from_slice(&VERSION.to_le_bytes());
    head[16..24].copy_from_slice(&event_id.to_le_bytes());
    let unreadable = u32::try_from(unreadable_folders).unwrap_or(u32::MAX);
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
    let lists = [
        &index.entry_name,
        &index.siblings.starts,
        &index.siblings.folders,
        &index.folders,
        &index.folder_inodes,
        &index.passages,
        &index.roots,
    ];
    let mut numbers = (index.name_starts.iter()).chain(lists.into_iter().flatten().copied());
    let mut buffer = Vec::with_capacity(CHUNK);
    loop {
        buffer.clear();
        buffer.extend(numbers.by_ref().take(CHUNK / 4).flat_map(u32::to_le_bytes));
        if buffer.is_empty() {
            break;
        }
        emit(&buffer)?;
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
        runs,
        folders,
        passages,
        roots,
    ] = [0, 4, 8, 12, 16, 20, 24, 28].map(|at| u64::from(number(SIZES + at)));
    // Checked before anything is allocated, so a damaged size never asks for gigabytes.
    let numbers = distinct + 1 + entries + 2 * runs + 2 * folders + passages + roots;
    if length != HEADER as u64 + settings_len + names_len + 4 * numbers {
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
    let run_starts = reader.numbers(runs)?;
    let run_folders = reader.numbers(runs)?;
    let folder_count = folders;
    let folders = reader.numbers(folder_count)?;
    let folder_inodes = reader.numbers(folder_count)?;
    let passages = reader.numbers(passages)?;
    let roots = reader.numbers(roots)?;
    if reader.crc.finalize() != saved_crc {
        return Err(OpenError::Damaged);
    }
    let names = String::from_utf8(names).map_err(|_| OpenError::Damaged)?;
    let runs = (run_starts, run_folders);
    let sound = is_sound(
        &names,
        &name_starts,
        &entry_name,
        (&runs.0, &runs.1),
        &folders,
        &passages,
        &roots,
    );
    let kept_starts = NameStarts::from_starts(&name_starts);
    drop(name_starts);
    let (true, Some(name_starts)) = (sound, kept_starts) else {
        return Err(OpenError::Damaged);
    };
    let mut index = Index::from_lists(
        names,
        name_starts,
        entry_name,
        runs,
        folders,
        folder_inodes,
        passages,
        roots,
    );
    // A root naming a folder above the one scanned would have the next update read all of it.
    let root = settings.root();
    let elsewhere = index.len() > 0 && index.path(0) != root
        || index.volumes().any(|volume| root.starts_with(volume));
    if elsewhere {
        return Err(OpenError::Damaged);
    }
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
    runs: (&[EntryId], &[u32]),
    folders: &[EntryId],
    passages: &[EntryId],
    roots: &[EntryId],
) -> bool {
    let distinct = name_starts.len() - 1;
    let entries = entry_name.len();
    let (starts, run_folders) = runs;
    let ascending = |list: &[EntryId]| list.windows(2).all(|pair| pair[0] < pair[1]);
    name_starts.first() == Some(&0)
        && name_starts.last().map(|&end| end as usize) == Some(names.len())
        && name_starts.windows(2).all(|pair| pair[0] <= pair[1])
        && name_starts
            .iter()
            .all(|&start| names.is_char_boundary(start as usize))
        && entry_name.iter().all(|&name| (name as usize) < distinct)
        && starts.len() == run_folders.len()
        && (entries == 0) == starts.is_empty()
        && starts.first().is_none_or(|&first| first == 0)
        && run_folders.first().is_none_or(|&root| root == NO_FOLDER)
        && ascending(starts)
        && starts.last().is_none_or(|&last| (last as usize) < entries)
        && ascending(folders)
        && folders.last().is_none_or(|&last| (last as usize) < entries)
        && ascending(passages)
        && passages
            .iter()
            .all(|passage| folders.binary_search(passage).is_ok())
        && (roots.first() == Some(&0) || (entries == 0 && roots.is_empty()))
        && ascending(roots)
        && roots.iter().all(|&root| {
            (root as usize) < entries
                && has_no_folder(starts, run_folders, root)
                && folders.binary_search(&root).is_ok()
        })
        && parents_are_folders_in_the_index(starts, run_folders, folders, roots)
}

/// Whether `entry`, inside the runs `starts` begin, is in a run of `NO_FOLDER`.
fn has_no_folder(starts: &[EntryId], run_folders: &[u32], entry: EntryId) -> bool {
    run_folders[starts.partition_point(|&s| s <= entry) - 1] == NO_FOLDER
}

/// Each run's folder is an earlier folder that an update has not removed, so every path
/// walks up to a root and removing a folder reaches everything inside it.
fn parents_are_folders_in_the_index(
    starts: &[EntryId],
    run_folders: &[u32],
    folders: &[EntryId],
    roots: &[EntryId],
) -> bool {
    starts.iter().zip(run_folders).all(|(&start, &slot)| {
        slot == NO_FOLDER
            || folders.get(slot as usize).is_some_and(|&parent| {
                parent < start
                    && (!has_no_folder(starts, run_folders, parent)
                        || roots.binary_search(&parent).is_ok())
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexBuilder, NO_PARENT};
    use crate::siblings::Siblings;
    use crate::walk::Kind;
    use crate::{Searcher, Within};

    /// The root of the made-up indexes below, as a saved list must name the folder scanned.
    const HOME: &str = "/";

    fn settings(root: &Path, clutter: Clutter) -> Settings {
        let home = Path::new(HOME);
        let mut settings = Settings::new(root, home, clutter, Ignore::default(), Access::Full);
        // Spelled as the made-up indexes spell it; Windows would make it a drive's root.
        if root == home {
            settings.root = home.to_path_buf();
        }
        settings
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
        write(
            &mut out,
            &scan.index,
            scan.unreadable_folders,
            settings,
            event_id,
        )
        .unwrap();
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

    /// The root's run, then one run in the root.
    const RUNS: &[(u32, u32)] = &[(0, NO_FOLDER), (1, 0)];

    /// Lists as given, `runs` as each run's start and folder place, with no depths or
    /// fingerprints, which saving does not read. Name starts out of order are saved wrapped
    /// around.
    fn raw(
        names: &str,
        starts: &[u32],
        name: &[u32],
        runs: &[(u32, u32)],
        folders: &[u32],
    ) -> Index {
        let mut name_starts = NameStarts::default();
        for (at, &start) in starts.iter().enumerate() {
            if at.div_ceil(16) == name_starts.groups.len() {
                name_starts.groups.push(start);
            }
            let group = name_starts.groups.last().unwrap();
            name_starts.offsets.push(start.wrapping_sub(*group) as u16);
        }
        Index {
            names: names.to_owned(),
            name_starts,
            name_letters: Vec::new(),
            name_digit_starts: Vec::new(),
            name_symbols: Vec::new(),
            entry_name: name.to_vec(),
            siblings: Siblings {
                starts: runs.iter().map(|&(start, _)| start).collect(),
                folders: runs.iter().map(|&(_, folder)| folder).collect(),
                ..Siblings::default()
            },
            roots: if name.is_empty() { Vec::new() } else { vec![0] },
            folders: folders.to_vec(),
            folder_inodes: vec![0; folders.len()],
            passages: Vec::new(),
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
        save_index(&file, &scanned.index, 12, &settings, u64::MAX - 1).unwrap();

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
            let mut parents = built.parents();
            // Entries an update removed, each with everything inside it, and volumes ejected.
            // Entry 1 joins the root's run when it goes.
            for _ in 0..next(20) {
                let entry = 1 + next(built.len());
                if entry < built.len() {
                    parents[entry] = NO_PARENT;
                }
            }
            if pass % 4 == 0 && built.len() > 1 {
                parents[1] = NO_PARENT;
            }
            if built.roots.len() > 1 && next(2) == 0 {
                built.roots.remove(1 + next(built.roots.len() - 1));
            }
            for entry in 1..built.len() {
                let parent = parents[entry];
                if parent != NO_PARENT
                    && !built.roots.contains(&parent)
                    && parents[parent as usize] == NO_PARENT
                {
                    parents[entry] = NO_PARENT;
                }
            }
            let runs = Siblings::from_parents(&parents, &built.folders);
            let roots = &built.roots;
            let gone =
                |entry: &EntryId| parents[*entry as usize] == NO_PARENT && !roots.contains(entry);
            let removed = (0..built.len() as EntryId).filter(gone).count();
            let removed_folders = built.folders.iter().filter(|folder| gone(folder)).count();
            let mut index = Index::from_lists(
                built.names,
                built.name_starts,
                built.entry_name,
                (runs.starts, runs.folders),
                built.folders,
                built.folder_inodes,
                built.passages,
                built.roots,
            );
            assert_eq!(
                (index.removed, index.removed_folders),
                (removed, removed_folders),
                "pass {pass}"
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
        let mut index = raw("/ab", &[0, 1, 3], &[0, 1], RUNS, &[0, 1]);
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
        assert_eq!(bytes[8..12], [4, 0, 0, 0]);
        let mut zeroed = bytes.clone();
        zeroed[12..16].fill(0);
        assert_eq!(bytes[12..16], crc32fast::hash(&zeroed).to_le_bytes());
        assert_eq!(bytes[16..24], [8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(bytes[24..28], [0x0D, 0x0C, 0x0B, 0x0A]);
        assert_eq!(bytes[28..32], [0x11, 0x10, 0x0F, 0x8E]);
        let sizes = [settings.key.len() as u32, 3, 2, 2, 2, 2, 1, 1];
        let expected: Vec<u8> = sizes.iter().flat_map(|size| size.to_le_bytes()).collect();
        assert_eq!(bytes[32..64], expected);
        assert_eq!(bytes[64..64 + settings.key.len()], settings.key);
        let lists = &bytes[64 + settings.key.len()..];
        assert_eq!(lists[..3], *b"/ab");
        assert_eq!(lists[3..15], [0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(lists[15..23], [0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(lists[23..31], [0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(lists[31..39], [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        assert_eq!(lists[39..47], [0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(lists[47..55], [4, 3, 2, 1, 5, 0, 0, 0]);
        assert_eq!(lists[55..59], [1, 0, 0, 0]);
        assert_eq!(lists[59..], [0, 0, 0, 0]);
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
        for at in (SIZES..HEADER).step_by(4) {
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
            3,
            5,
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
        save_index(&file, &scan.index, 0, &older, 1).unwrap();
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
            let mut index = raw("/aé", &[0, 1, 4], &[0, 1], RUNS, &[0, 1]);
            index.passages = passages.to_vec();
            index
        };
        // The root, a volume and a file on it.
        let with_roots = |runs: &[(u32, u32)], folders: &[u32], roots: &[u32]| {
            let mut index = raw("/abc", &[0, 1, 2, 4], &[0, 1, 2], runs, folders);
            index.roots = roots.to_vec();
            index
        };
        let volume = [(0, NO_FOLDER), (2, 1)];
        let sound = [
            raw("/aé", &[0, 1, 4], &[0, 1], RUNS, &[0]),
            // Removed by an update, in a run of its own or the root's.
            raw(
                "/aé",
                &[0, 1, 4],
                &[0, 1],
                &[(0, NO_FOLDER), (1, NO_FOLDER)],
                &[0],
            ),
            raw("/aé", &[0, 1, 4], &[0, 1], &[(0, NO_FOLDER)], &[0]),
            with_passages(&[1]),
            with_roots(&volume, &[0, 1], &[0, 1]),
            // An ejected volume, everything on it removed.
            with_roots(&[(0, NO_FOLDER)], &[0, 1], &[0]),
        ];
        let unsound_roots = [
            // A volume named as the root, or above it, would have an update read the root again.
            {
                let mut twins = with_roots(&volume, &[0, 1], &[0, 1]);
                twins.entry_name[1] = twins.entry_name[0];
                twins
            },
            with_roots(&volume, &[0, 1], &[1]),
            with_roots(&volume, &[0, 1], &[]),
            with_roots(&volume, &[0, 1], &[0, 1, 1]),
            with_roots(&volume, &[0, 1], &[0, 3]),
            with_roots(&volume, &[0, 1], &[0, EntryId::MAX]),
            with_roots(&volume, &[0], &[0, 1]),
            with_roots(&[(0, NO_FOLDER), (1, 0), (2, 1)], &[0, 1], &[0, 1]),
            // A file left on an ejected volume.
            with_roots(&volume, &[0, 1], &[0]),
            {
                let mut empty = raw("", &[0], &[], &[], &[]);
                empty.roots = vec![0];
                empty
            },
        ];
        let unsound = [
            raw("/aé", &[0, 1, 3], &[0, 1], RUNS, &[0]),
            raw("/aé", &[1, 1, 4], &[0, 1], RUNS, &[0]),
            raw("/aé", &[0, 2, 1, 4], &[0, 1], RUNS, &[0]),
            raw("/aé", &[0, 3, 4], &[0, 1], RUNS, &[0]),
            raw("/aé", &[0, 1, 4], &[0, 2], RUNS, &[0]),
            // The root inside a folder, and an entry inside itself.
            raw("/aé", &[0, 1, 4], &[0, 1], &[(0, 0), (1, 0)], &[0]),
            raw(
                "/aé",
                &[0, 1, 4],
                &[0, 1],
                &[(0, NO_FOLDER), (1, 1)],
                &[0, 1],
            ),
            raw("/aé", &[0, 1, 4], &[0, 1], RUNS, &[0, 0]),
            raw("/aé", &[0, 1, 4], &[0, 1], RUNS, &[2]),
            raw("", &[0], &[], &[], &[0]),
            // Runs that miss entries, overlap or run past the end.
            raw("/aé", &[0, 1, 4], &[0, 1], &[], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[(1, NO_FOLDER)], &[0]),
            raw(
                "/aé",
                &[0, 1, 4],
                &[0, 1],
                &[(0, NO_FOLDER), (1, 0), (1, 0)],
                &[0],
            ),
            raw("/aé", &[0, 1, 4], &[0, 1], &[(0, NO_FOLDER), (2, 0)], &[0]),
            with_passages(&[1, 1]),
            with_passages(&[2]),
            with_passages(&[NO_PARENT]),
            // A folder that is not there, and an entry left inside a removed folder.
            raw(
                "/abc",
                &[0, 1, 2, 4],
                &[0, 1, 2],
                &[(0, NO_FOLDER), (1, 0), (2, 1)],
                &[0],
            ),
            raw(
                "/abc",
                &[0, 1, 2, 4],
                &[0, 1, 2],
                &[(0, NO_FOLDER), (2, 1)],
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
    fn refuses_a_name_too_far_from_its_group_s_start() {
        let settings = keep(Path::new(HOME));
        // After the root's name, a group's names span at most 65,535 bytes.
        let names = |long: u32| format!("/{}b", "a".repeat(long as usize));
        let index = |long: u32| {
            let starts = [0, 1, long + 1, long + 2];
            let index = raw(&names(long), &starts, &[0, 1, 2], RUNS, &[0]);
            encode(&whole(index), &settings, 0)
        };
        let fits = u32::from(u16::MAX) - 1;
        assert!(decode(&index(fits), &settings).is_ok());
        // Saving the start too far wraps it around, so it is written as it was meant.
        let mut bytes = index(fits + 1);
        let end = HEADER + settings.key.len() + names(fits + 1).len() + 4 * 3;
        bytes[end..end + 4].copy_from_slice(&(fits + 3).to_le_bytes());
        fix_crc(&mut bytes);
        assert!(matches!(decode(&bytes, &settings), Err(OpenError::Damaged)));
    }

    #[test]
    fn opens_the_longest_names_a_disk_allows_and_folders_deeper_than_depths_count() {
        let settings = keep(Path::new(HOME));
        let mut builder = IndexBuilder::new();
        let mut folder = builder.push("/", NO_PARENT, true);
        // 255 characters of 3 bytes each, as macOS allows, fill whole groups of name starts.
        for i in 0..40 {
            let long: String =
                std::iter::repeat_n(char::from_u32(0x65E5 + i).unwrap(), 255).collect();
            builder.push(&long, folder, false);
        }
        for _ in 0..260 {
            folder = builder.push("d", folder, true);
        }
        let built = whole(builder.finish());
        let saved = decode(&encode(&built, &settings, 0), &settings).unwrap();
        let index = saved.scan.index;
        assert_eq!(index, built.index);
        assert_eq!(index.name(40).len(), 765);
        assert_eq!(index.depth(40), 1);
        assert_eq!(index.depth(index.len() as EntryId - 1), u8::MAX);
    }

    #[test]
    #[should_panic(expected = "longer than 765 bytes")]
    fn a_name_far_longer_than_a_disk_allows_is_refused_while_building() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/", NO_PARENT, true);
        for i in 0..16 {
            builder.push(&format!("{i}{}", "a".repeat(5000)), root, false);
        }
    }

    /// Damaged with the CRC intact, as a bug in saving would leave it: the next update would read
    /// the whole folder the root names.
    #[test]
    fn refuses_a_root_that_names_another_folder() {
        let (dir, settings, bytes) = saved_tree();
        assert!(decode(&bytes, &settings).is_ok());
        let root = fs::canonicalize(dir.path()).unwrap();
        let name = root.to_str().unwrap().as_bytes();
        let names_at = HEADER + settings.key.len();
        assert_eq!(&bytes[names_at..names_at + name.len()], name);
        let mut elsewhere = bytes.clone();
        // The same length, so only the folder it names differs.
        elsewhere[names_at + name.len() - 1] ^= 1;
        fix_crc(&mut elsewhere);
        assert!(matches!(
            decode(&elsewhere, &settings),
            Err(OpenError::Damaged)
        ));
        let other = raw("/x", &[0, 2], &[0], &[(0, NO_FOLDER)], &[0]);
        let other = encode(&whole(other), &keep(Path::new(HOME)), 0);
        assert!(matches!(
            decode(&other, &keep(Path::new(HOME))),
            Err(OpenError::Damaged)
        ));
    }

    #[test]
    fn refuses_names_that_are_not_utf_8() {
        let settings = keep(Path::new(HOME));
        let index = raw("/ab", &[0, 1, 3], &[0, 1], RUNS, &[0]);
        let mut bytes = encode(&whole(index), &settings, 0);
        bytes[HEADER + settings.key.len() + 1] = 0xFF;
        fix_crc(&mut bytes);
        assert!(matches!(decode(&bytes, &settings), Err(OpenError::Damaged)));
    }

    /// Lists damaged so the CRC still holds, as a bug in saving would leave them: either
    /// refused, or safe to show, search, search inside a folder, update and eject from.
    fn assert_damage_never_panics(source: &str, settings: &Settings, bytes: &[u8], least: usize) {
        let root = settings.root().to_path_buf();
        let volumes: Vec<PathBuf> = (decode(bytes, settings).unwrap().scan.index.volumes())
            .map(Path::to_path_buf)
            .collect();
        let mut changed = vec![root.clone(), root.join("src"), root.join("docs/README.md")];
        for volume in &volumes {
            changed.extend([volume.clone(), volume.join("src")]);
        }
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
            let mut damaged = bytes.to_vec();
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
            let Ok(saved) = decode(&damaged, settings) else {
                continue;
            };
            opened += 1;
            let mut index = saved.scan.index;
            let apps = crate::Apps::default();
            let search = |index: &Index, within: Option<&Within>, query: &str| {
                let hits = searcher.search_in(index, &apps, within, query, searcher.ticket(), 10);
                assert!(hits.is_some(), "{source}, pass {pass}");
            };
            for entry in index.ids() {
                let _ = (index.path(entry), index.name(entry), index.is_dir(entry));
                let _ = index.is_removed(entry);
            }
            for query in ["a", "re", "md src", "é"] {
                search(&index, None, query);
            }
            let mut folders = [std::slice::from_ref(&root), &volumes].concat();
            // Picked without the seed, so each source damages its lists as before.
            if index.len() > 0 {
                folders.extend((1..4).map(|at| index.path((pass * at % index.len()) as EntryId)));
            }
            for folder in &folders {
                if let Some(within) = Within::folder(&index, folder) {
                    search(&index, Some(&within), "re");
                }
            }
            settings.update(&mut index, &changed);
            search(&index, None, "md src");
            for volume in &volumes {
                settings.remove_volume(&mut index, volume);
            }
            search(&index, None, "md src");
        }
        assert!(
            opened > least,
            "{source}: only {opened} damaged indexes opened"
        );
    }

    #[test]
    fn damaged_lists_with_a_good_crc_never_panic() {
        let (_dir, settings, bytes) = saved_tree();
        assert_damage_never_panics("a plain scan", &settings, &bytes, 500);
    }

    #[test]
    fn damaged_lists_holding_a_volume_never_panic() {
        let (home, volume) = (tree(), tree());
        let settings = keep(home.path());
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, volume.path()).unwrap();
        let bytes = encode(&whole(index), &settings, 7);
        assert_damage_never_panics("a volume", &settings, &bytes, 500);
    }

    /// A volume ejected and a folder and a file deleted, their entries left in place as too
    /// few to rebuild the lists for, beside a volume still plugged in.
    #[test]
    fn damaged_lists_holding_removed_entries_never_panic() {
        let (home, volume, ejected) = (tree(), tree(), tempfile::tempdir().unwrap());
        let root = fs::canonicalize(home.path()).unwrap();
        fs::create_dir(root.join("pad")).unwrap();
        for i in 0..150 {
            fs::write(root.join("pad").join(i.to_string()), "").unwrap();
        }
        fs::write(ejected.path().join("a.txt"), "").unwrap();
        let settings = keep(&root);
        let mut index = settings.scan().unwrap().index;
        settings.add_volume(&mut index, ejected.path()).unwrap();
        settings.add_volume(&mut index, volume.path()).unwrap();
        let ejected = fs::canonicalize(ejected.path()).unwrap();
        settings.remove_volume(&mut index, &ejected).unwrap();
        fs::remove_dir(root.join("empty")).unwrap();
        fs::remove_file(root.join("docs/README.md")).unwrap();
        settings.update(&mut index, &[root.clone(), root.join("docs")]);
        assert_eq!((index.removed, index.volumes().count()), (4, 1));
        let bytes = encode(&whole(index), &settings, 7);
        assert_damage_never_panics("removed entries", &settings, &bytes, 500);
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

        save_index(&file, &scan.index, 0, &all, 3).unwrap();
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
    fn opens_only_under_the_journal_it_was_saved_under() {
        let dir = tree();
        let file = dir.path().join("index");
        let settings = keep(dir.path());
        let scan = settings.scan().unwrap();
        let journal = settings.clone().with_journal(b"5F2A0C1E");
        save_index(&file, &scan.index, 0, &journal, 7).unwrap();
        assert_eq!(open_index(&file, &journal).unwrap().event_id, 7);
        assert_eq!(journal, settings.clone().with_journal(b"5F2A0C1E"));
        for other in [
            settings.clone(),
            settings.clone().with_journal(b"5F2A0C1F"),
            settings.clone().with_journal(b""),
            journal.clone().with_journal(b"5F2A0C1E"),
        ] {
            let opened = open_index(&file, &other);
            assert!(matches!(opened, Err(OpenError::OtherSettings)));
        }
    }

    #[test]
    fn replaces_the_old_file_and_leaves_no_temporary_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let index = raw("/", &[0, 1], &[0], &[(0, NO_FOLDER)], &[0]);
        let settings = keep(Path::new(HOME));
        save_index(&file, &index, 0, &settings, 1).unwrap();
        save_index(&file, &index, 0, &settings, 2).unwrap();
        assert_eq!(open_index(&file, &settings).unwrap().event_id, 2);
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);

        let nowhere = dir.path().join("missing").join("index");
        assert!(save_index(&nowhere, &index, 0, &settings, 1).is_err());
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn removes_only_old_temporary_files_of_its_own_list() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let hour = Duration::from_secs(3600);
        let names = [
            "index.4123.tmp",
            "index.4124.tmp",
            "index",
            "index.tmp",
            "index.12a.tmp",
            "index.4125.tmp.old",
            "other.4123.tmp",
            "indexes.4123.tmp",
        ];
        for name in names {
            let made = File::create(dir.path().join(name)).unwrap();
            if name != "index.4124.tmp" {
                made.set_modified(std::time::SystemTime::now() - 2 * hour)
                    .unwrap();
            }
        }
        remove_old_temps(&file, hour);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        // Another copy of the app may still be writing the one an hour hasn't passed for.
        let mut kept = names[1..].to_vec();
        kept.sort();
        assert_eq!(left, kept);

        remove_old_temps(&dir.path().join("missing/index"), hour);
        remove_old_temps(Path::new("/"), hour);
    }

    #[test]
    fn a_save_that_fails_late_removes_its_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        fs::create_dir(&file).unwrap();
        fs::write(file.join("inside"), "mine").unwrap();
        let index = raw("/", &[0, 1], &[0], &[(0, NO_FOLDER)], &[0]);

        assert!(save_index(&file, &index, 0, &keep(dir.path()), 1).is_err());
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
        let index = raw("/", &[0, 1], &[0], &[(0, NO_FOLDER)], &[0]);

        let saved = save_index(&file, &index, 0, &keep(dir.path()), 1);
        assert_eq!(saved.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&temp).unwrap(), "mine");
        assert!(!file.exists());
    }
}
