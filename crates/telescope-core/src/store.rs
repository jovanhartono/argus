//! The saved index: one file that opens in milliseconds, so a restart needs no rescan.
//!
//! Version 1, every number little-endian:
//!
//! | bytes | what                                                       |
//! |-------|------------------------------------------------------------|
//! | 0     | `TL-INDEX`                                                 |
//! | 8     | format version, u32                                        |
//! | 12    | CRC-32 of the whole file, read with these 4 bytes as zeros |
//! | 16    | FSEvents event id the index is current to, u64             |
//! | 24    | sizes, u32 each: settings bytes, name bytes, distinct      |
//! |       | names, entries, folders                                    |
//! | 44    | settings, names, name starts, entry names, entry parents,  |
//! |       | folders; the last four as u32 lists                        |

use crate::Ignore;
use crate::index::{EntryId, Index, NO_PARENT, NameId};
use crate::walk::{Access, Clutter, Scan, scan};
use crc32fast::Hasher;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: [u8; 8] = *b"TL-INDEX";
const VERSION: u32 = 1;
const HEADER: usize = 44;
const CRC: std::ops::Range<usize> = 12..16;
/// Lists pass through a buffer this size, so saving or opening never holds a second copy.
const CHUNK: usize = 256 * 1024;

/// What an index was built from, the same values `scan` takes. A saved index opens only with
/// the same settings, so turning on `--all`, editing the ignore list, granting Full Disk
/// Access or updating Telescope costs one rescan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings(Vec<u8>);

impl Settings {
    pub fn new(
        root: &Path,
        home: &Path,
        clutter: Clutter,
        ignore: &Ignore,
        access: Access,
    ) -> Self {
        let mut bytes = Vec::new();
        put(&mut bytes, env!("CARGO_PKG_VERSION").as_bytes());
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        put(&mut bytes, root.as_os_str().as_encoded_bytes());
        put(&mut bytes, home.as_os_str().as_encoded_bytes());
        bytes.push(match access {
            Access::Full => 0,
            Access::Limited => 1,
        });
        match clutter {
            Clutter::Keep => bytes.push(0),
            Clutter::Skip => {
                bytes.push(1);
                ignore.encode(&mut bytes);
            }
        }
        Self(bytes)
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

pub struct Saved {
    pub index: Index,
    /// The FSEvents event the index was current to when saved, so changes since can be caught up.
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
pub fn open_or_scan(
    file: &Path,
    root: &Path,
    home: &Path,
    clutter: Clutter,
    ignore: &Ignore,
    access: Access,
) -> io::Result<Opened> {
    let settings = Settings::new(root, home, clutter, ignore, access);
    match open_index(file, &settings) {
        Ok(saved) => Ok(Opened::Saved(saved)),
        Err(why) => Ok(Opened::Scanned {
            scan: scan(root, home, clutter, ignore, access)?,
            why,
        }),
    }
}

/// Written beside `file` and renamed over it, so a crash mid-save leaves the old file whole.
pub fn save_index(
    file: &Path,
    index: &Index,
    settings: &Settings,
    event_id: u64,
) -> io::Result<()> {
    let temp = file.with_added_extension("tmp");
    let saved = write(&temp, index, settings, event_id).and_then(|()| fs::rename(&temp, file));
    if saved.is_err() {
        let _ = fs::remove_file(&temp);
    }
    saved
}

fn write(temp: &Path, index: &Index, settings: &Settings, event_id: u64) -> io::Result<()> {
    let sizes = [
        settings.0.len(),
        index.names.len(),
        index.distinct_names(),
        index.len(),
        index.folders.len(),
    ];
    let mut head = [0; HEADER];
    head[..8].copy_from_slice(&MAGIC);
    head[8..12].copy_from_slice(&VERSION.to_le_bytes());
    head[16..24].copy_from_slice(&event_id.to_le_bytes());
    for (at, size) in head[24..].as_chunks_mut().0.iter_mut().zip(sizes) {
        let size = u32::try_from(size).map_err(|_| io::Error::other("index too large to save"))?;
        *at = size.to_le_bytes();
    }

    let mut crc = Hasher::new();
    crc.update(&head);
    let mut out = BufWriter::with_capacity(CHUNK, File::create(temp)?);
    out.write_all(&head)?;
    let mut emit = |bytes: &[u8]| {
        crc.update(bytes);
        out.write_all(bytes)
    };
    emit(&settings.0)?;
    emit(index.names.as_bytes())?;
    let mut buffer = Vec::with_capacity(CHUNK);
    for list in [
        &index.name_starts,
        &index.entry_name,
        &index.entry_parent,
        &index.folders,
    ] {
        for chunk in list.chunks(CHUNK / 4) {
            buffer.clear();
            buffer.extend(chunk.iter().flat_map(|number| number.to_le_bytes()));
            emit(&buffer)?;
        }
    }
    head[CRC].copy_from_slice(&crc.finalize().to_le_bytes());
    out.seek(SeekFrom::Start(0))?;
    out.write_all(&head)?;
    out.into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()
}

pub fn open_index(file: &Path, settings: &Settings) -> Result<Saved, OpenError> {
    let mut file = File::open(file)?;
    let length = file.metadata()?.len();
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
    let [settings_len, names_len, distinct, entries, folders] =
        [24, 28, 32, 36, 40].map(|at| u64::from(number(at)));
    // Checked before anything is allocated, so a damaged size never asks for gigabytes.
    let body = settings_len + names_len + 4 * (distinct + 1 + 2 * entries + folders);
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
    if reader.bytes(settings_len)? != settings.0 {
        return Err(OpenError::OtherSettings);
    }
    let names = reader.bytes(names_len)?;
    let name_starts = reader.numbers(distinct + 1)?;
    let entry_name = reader.numbers(entries)?;
    let entry_parent = reader.numbers(entries)?;
    let folders = reader.numbers(folders)?;
    if reader.crc.finalize() != saved_crc {
        return Err(OpenError::Damaged);
    }
    let names = String::from_utf8(names).map_err(|_| OpenError::Damaged)?;
    if !is_sound(&names, &name_starts, &entry_name, &entry_parent, &folders) {
        return Err(OpenError::Damaged);
    }
    Ok(Saved {
        index: Index::from_lists(names, name_starts, entry_name, entry_parent, folders),
        event_id,
    })
}

struct Reader {
    file: File,
    crc: Hasher,
    buffer: Vec<u8>,
}

impl Reader {
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

/// Every number points inside its list, so no search can step out of bounds, and every path
/// walks up to the root.
fn is_sound(
    names: &str,
    name_starts: &[u32],
    entry_name: &[NameId],
    entry_parent: &[EntryId],
    folders: &[EntryId],
) -> bool {
    let distinct = name_starts.len() - 1;
    name_starts.first() == Some(&0)
        && name_starts.last().map(|&end| end as usize) == Some(names.len())
        && name_starts.windows(2).all(|pair| pair[0] <= pair[1])
        && name_starts
            .iter()
            .all(|&start| names.is_char_boundary(start as usize))
        && entry_name.iter().all(|&name| (name as usize) < distinct)
        && entry_parent.first().is_none_or(|&root| root == NO_PARENT)
        && entry_parent
            .iter()
            .enumerate()
            .skip(1)
            .all(|(entry, &parent)| (parent as usize) < entry)
        && folders.windows(2).all(|pair| pair[0] < pair[1])
        && folders
            .last()
            .is_none_or(|&last| (last as usize) < entry_parent.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexBuilder;
    use std::path::PathBuf;

    const HOME: &str = "/nowhere";

    fn keep(root: &Path) -> Settings {
        let home = Path::new(HOME);
        Settings::new(root, home, Clutter::Keep, &Ignore::default(), Access::Full)
    }

    fn scan_all(root: &Path) -> Index {
        let home = Path::new(HOME);
        let scanned = scan(root, home, Clutter::Keep, &Ignore::default(), Access::Full);
        scanned.unwrap().index
    }

    fn open_or_scan_all(file: &Path, root: &Path) -> io::Result<Opened> {
        let home = Path::new(HOME);
        open_or_scan(
            file,
            root,
            home,
            Clutter::Keep,
            &Ignore::default(),
            Access::Full,
        )
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "docs/README.md",
            "src/main.rs",
            "src/README.md",
            "café/naïve résumé.pdf",
        ] {
            let file = dir.path().join(file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "").unwrap();
        }
        fs::create_dir(dir.path().join("empty")).unwrap();
        dir
    }

    /// A small index saved in `dir`, and the file's bytes.
    fn saved_bytes(dir: &Path) -> (PathBuf, Vec<u8>) {
        let file = dir.join("index");
        save_index(&file, &scan_all(dir), &keep(dir), 7).unwrap();
        let bytes = fs::read(&file).unwrap();
        (file, bytes)
    }

    /// Lists as given, with no depths or fingerprints, which saving does not read.
    fn raw(names: &str, starts: &[u32], name: &[u32], parent: &[u32], folders: &[u32]) -> Index {
        Index {
            names: names.to_owned(),
            name_starts: starts.to_vec(),
            name_letters: Vec::new(),
            entry_name: name.to_vec(),
            entry_parent: parent.to_vec(),
            entry_depth: Vec::new(),
            folders: folders.to_vec(),
        }
    }

    #[test]
    fn opens_what_it_saved() {
        let dir = tree();
        let file = dir.path().join("index");
        let scanned = scan_all(dir.path());
        save_index(&file, &scanned, &keep(dir.path()), u64::MAX - 1).unwrap();

        let saved = open_index(&file, &keep(dir.path())).unwrap();
        assert_eq!(saved.index, scanned);
        assert_eq!(saved.event_id, u64::MAX - 1);
        let names: Vec<_> = (0..saved.index.len() as EntryId)
            .map(|entry| saved.index.name(entry))
            .collect();
        assert!(names.contains(&"naïve résumé.pdf"));
    }

    #[test]
    fn opens_what_it_saved_for_many_made_up_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let settings = keep(dir.path());
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        let letters = ["a", "B", "é", ".", " ", "z", "日本", "-"];
        for _ in 0..200 {
            let mut builder = IndexBuilder::new();
            let mut folders = vec![builder.push("/", NO_PARENT, true)];
            for _ in 0..next(300) {
                let name: String = (0..next(6)).map(|_| letters[next(letters.len())]).collect();
                let parent = folders[next(folders.len())];
                let is_dir = next(3) == 0;
                let entry = builder.push(&name, parent, is_dir);
                if is_dir {
                    folders.push(entry);
                }
            }
            let index = builder.finish();
            let event_id = (next(usize::MAX) as u64) << 1;
            save_index(&file, &index, &settings, event_id).unwrap();
            let saved = open_index(&file, &settings).unwrap();
            assert_eq!(saved.index, index);
            assert_eq!(saved.event_id, event_id);
        }
    }

    #[test]
    fn saves_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let empty = IndexBuilder::new().finish();
        save_index(&file, &empty, &keep(dir.path()), 0).unwrap();
        assert_eq!(open_index(&file, &keep(dir.path())).unwrap().index, empty);
    }

    #[test]
    fn numbers_are_little_endian_at_fixed_places() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let index = raw("/ab", &[0, 1, 3], &[0, 1], &[NO_PARENT, 0], &[0]);
        let settings = keep(dir.path());
        save_index(&file, &index, &settings, 0x0102_0304_0506_0708).unwrap();
        let bytes = fs::read(&file).unwrap();

        assert_eq!(bytes[..8], *b"TL-INDEX");
        assert_eq!(bytes[8..12], [1, 0, 0, 0]);
        assert_eq!(bytes[16..24], [8, 7, 6, 5, 4, 3, 2, 1]);
        let sizes = [settings.0.len() as u32, 3, 2, 2, 1];
        let expected: Vec<u8> = sizes.iter().flat_map(|size| size.to_le_bytes()).collect();
        assert_eq!(bytes[24..44], expected);
        let lists = &bytes[44 + settings.0.len()..];
        assert_eq!(lists[..3], *b"/ab");
        assert_eq!(lists[3..15], [0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0]);
        assert_eq!(lists.len(), 3 + 4 * (3 + 2 + 2 + 1));
    }

    #[test]
    fn refuses_a_file_cut_short_at_any_length() {
        let dir = tree();
        let (file, bytes) = saved_bytes(dir.path());
        for length in 0..bytes.len() {
            fs::write(&file, &bytes[..length]).unwrap();
            let opened = open_index(&file, &keep(dir.path()));
            assert!(
                matches!(opened, Err(OpenError::WrongLength)),
                "length {length}"
            );
        }
        fs::write(&file, [bytes.as_slice(), &[0]].concat()).unwrap();
        let opened = open_index(&file, &keep(dir.path()));
        assert!(matches!(opened, Err(OpenError::WrongLength)));
    }

    #[test]
    fn refuses_any_flipped_bit() {
        let dir = tree();
        let (file, bytes) = saved_bytes(dir.path());
        let settings = keep(dir.path());
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1 << bit;
                fs::write(&file, &damaged).unwrap();
                assert!(open_index(&file, &settings).is_err(), "byte {at} bit {bit}");
            }
        }
        fs::write(&file, &bytes).unwrap();
        assert!(open_index(&file, &settings).is_ok());
    }

    #[test]
    fn refuses_other_versions_and_other_files() {
        let dir = tree();
        let (file, bytes) = saved_bytes(dir.path());
        for version in [0, 2, u32::MAX, u32::from_be_bytes(VERSION.to_le_bytes())] {
            let mut old = bytes.clone();
            old[8..12].copy_from_slice(&version.to_le_bytes());
            fs::write(&file, &old).unwrap();
            let opened = open_index(&file, &keep(dir.path()));
            assert!(matches!(opened, Err(OpenError::Version(v)) if v == version));
        }
        let mut other = bytes.clone();
        other[..8].copy_from_slice(b"bplist00");
        fs::write(&file, &other).unwrap();
        let opened = open_index(&file, &keep(dir.path()));
        assert!(matches!(opened, Err(OpenError::NotAnIndex)));
        let opened = open_index(&dir.path().join("missing"), &keep(dir.path()));
        assert!(matches!(opened, Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn opens_only_with_the_settings_it_was_built_with() {
        let dir = tree();
        let (file, _) = saved_bytes(dir.path());
        let root = dir.path();
        let home = Path::new(HOME);
        let none = &Ignore::default();
        let (ignore, _) = Ignore::parse("docs\n", root, Access::Full);
        let other = [
            Settings::new(root, home, Clutter::Skip, none, Access::Full),
            Settings::new(&root.join("src"), home, Clutter::Keep, none, Access::Full),
            Settings::new(root, root, Clutter::Keep, none, Access::Full),
            Settings::new(root, home, Clutter::Keep, none, Access::Limited),
        ];
        for settings in other {
            let opened = open_index(&file, &settings);
            assert!(matches!(opened, Err(OpenError::OtherSettings)));
        }
        assert_ne!(
            Settings::new(root, home, Clutter::Skip, &ignore, Access::Full),
            Settings::new(root, home, Clutter::Skip, none, Access::Full)
        );
        // --all leaves the ignore list out, so editing it changes nothing.
        let edited = Settings::new(root, home, Clutter::Keep, &ignore, Access::Full);
        assert!(open_index(&file, &edited).is_ok());
        let dotted = root.join("src").join("..");
        assert!(open_index(&file, &keep(&dotted)).is_ok());
    }

    #[test]
    fn refuses_lists_that_point_outside_themselves() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let settings = keep(dir.path());
        let sound = raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]);
        let unsound = [
            raw("/aé", &[0, 1, 3], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[1, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 2, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 3, 4], &[0, 1], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 2], &[NO_PARENT, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[0, 0], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 1], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, NO_PARENT], &[0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[0, 0]),
            raw("/aé", &[0, 1, 4], &[0, 1], &[NO_PARENT, 0], &[2]),
        ];
        save_index(&file, &sound, &settings, 0).unwrap();
        assert!(open_index(&file, &settings).is_ok());
        for (i, index) in unsound.iter().enumerate() {
            save_index(&file, index, &settings, 0).unwrap();
            let opened = open_index(&file, &settings);
            assert!(matches!(opened, Err(OpenError::Damaged)), "case {i}");
        }
    }

    #[test]
    fn scans_when_the_saved_index_cannot_be_used() {
        let dir = tree();
        let root = dir.path();
        let file = root.join("index");
        let opened = open_or_scan_all(&file, root).unwrap();
        let Opened::Scanned { scan, why } = opened else {
            panic!("opened a file that is not there");
        };
        assert!(matches!(why, OpenError::Io(_)));

        save_index(&file, &scan.index, &keep(root), 3).unwrap();
        let opened = open_or_scan_all(&file, root).unwrap();
        let Opened::Saved(saved) = opened else {
            panic!("rescanned a good file");
        };
        assert_eq!(saved.index, scan.index);
        assert_eq!(saved.event_id, 3);

        fs::write(&file, "garbage").unwrap();
        let opened = open_or_scan_all(&file, root).unwrap();
        assert!(matches!(
            opened,
            Opened::Scanned {
                why: OpenError::WrongLength,
                ..
            }
        ));
        assert!(open_or_scan_all(&file, &root.join("missing")).is_err());
    }

    #[test]
    fn replaces_the_old_file_and_leaves_no_temporary_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let index = raw("/", &[0, 1], &[0], &[NO_PARENT], &[0]);
        let settings = keep(dir.path());
        save_index(&file, &index, &settings, 1).unwrap();
        save_index(&file, &index, &settings, 2).unwrap();
        assert_eq!(open_index(&file, &settings).unwrap().event_id, 2);
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);

        let nowhere = dir.path().join("missing").join("index");
        assert!(save_index(&nowhere, &index, &settings, 1).is_err());
        assert!(!dir.path().join("missing").exists());
    }
}
