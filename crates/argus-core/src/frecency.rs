//! Frecency: files opened often and lately rank higher.
//!
//! Each open adds 1 to a file's score, and a score halves every 30 days. A file keeps one
//! number, `at`: the moment its score equals 1, so its score at `now` is
//! 2^((at - now) / 30 days). A later `at` is a higher score at every moment, so nothing needs
//! updating as time passes.
//!
//! The saved list, version 1, every number little-endian:
//!
//! | bytes | what                                                       |
//! |-------|------------------------------------------------------------|
//! | 0     | `TL-OPENS`                                                 |
//! | 8     | format version, u32                                        |
//! | 12    | CRC-32 of the whole file, read with these 4 bytes as zeros |
//! | 16    | files, u32                                                 |
//! | 20    | per file, by path: `at` in seconds since 1970 as f64, the  |
//! |       | path's length as u16, the path in UTF-8                    |

use crate::apps::{AppId, Apps};
use crate::index::{EntryId, Index, NO_PARENT, NameId, name_fingerprint};
use crate::search::Item;
use crate::siblings::NO_FOLDER;
use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;
use std::f64::consts::LN_2;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: [u8; 8] = *b"TL-OPENS";
const VERSION: u32 = 1;
const HEADER: usize = 20;
const CRC: std::ops::Range<usize> = 12..16;
/// An open 30 days ago counts half as much as one today.
const HALF_LIFE: f64 = 30.0 * 24.0 * 60.0 * 60.0;
/// A file is forgotten once its score falls under 1/16: one open, four months on.
const FORGET_AFTER: f64 = 4.0 * HALF_LIFE;
/// A score stops growing at 1024, as if opened 1024 times at once. One further ahead, by more
/// than a month's slack, was counted by a clock that ran ahead and counts as one open now:
/// otherwise that file would top every search for years.
const MOST_AHEAD: f64 = 10.0 * HALF_LIFE;
const CLOCK_SLACK: f64 = HALF_LIFE;
const MOST_FILES: usize = 1024;
const LONGEST_PATH: usize = 4096;
const MOST_BYTES: usize = HEADER + MOST_FILES * (10 + LONGEST_PATH);
const CHUNK: usize = 65_536;
/// In search's units, where a typed letter is worth 12: an open just now is worth a letter,
/// and each doubling of the score one more, up to four.
const BOOST_PER_DOUBLING: f64 = 12.0;
const MOST_BOOST: f64 = 48.0;

/// Saves in this process take turns, so a temporary file named after this process is never
/// one still being written: an earlier process with the same number left it.
static SAVING: Mutex<()> = Mutex::new(());

/// The files opened from Argus, by full path, and when each counts as one open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frecency {
    at: HashMap<Box<str>, f64>,
}

/// Why a saved list could not be read.
#[derive(Debug)]
pub enum FrecencyError {
    Io(io::Error),
    NotAList,
    Version(u32),
    WrongLength,
    Damaged,
}

impl fmt::Display for FrecencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::NotAList => f.write_str("not an Argus list of opened files"),
            Self::Version(version) => {
                write!(f, "saved as version {version}, this build reads {VERSION}")
            }
            Self::WrongLength => f.write_str("wrong length, likely cut short"),
            Self::Damaged => f.write_str("damaged"),
        }
    }
}

impl std::error::Error for FrecencyError {}

impl FrecencyError {
    /// A list cut short or damaged, which saving a new list over loses nothing. Anything else,
    /// a list from a newer build, another kind of file or one that could not be read, is kept.
    pub fn is_damaged(&self) -> bool {
        matches!(self, Self::WrongLength | Self::Damaged)
    }
}

impl From<io::Error> for FrecencyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Seconds since 1970, the clock `Frecency` counts in.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn healed(at: f64, now: f64) -> f64 {
    if at - now > MOST_AHEAD + CLOCK_SLACK {
        now
    } else {
        at
    }
}

fn score(at: f64, now: u64) -> f64 {
    let now = now as f64;
    ((healed(at, now) - now) / HALF_LIFE).exp2()
}

/// `at` after one more open at `now`: the score at `now` plus 1, as a new `at`.
fn add_open(at: f64, now: f64) -> f64 {
    let (newer, older) = if at > now { (at, now) } else { (now, at) };
    let at = newer + HALF_LIFE * ((older - newer) / HALF_LIFE).exp2().ln_1p() / LN_2;
    at.min(now + MOST_AHEAD)
}

fn boost(score: f64) -> u32 {
    // A list is pruned only on an open, so one left alone still holds files it has forgotten.
    if score < (-FORGET_AFTER / HALF_LIFE).exp2() {
        return 0;
    }
    (BOOST_PER_DOUBLING * score.ln_1p() / LN_2)
        .min(MOST_BOOST)
        .round() as u32
}

/// One spelling per path, so "/a/b/" and "/a//b" count as opens of "/a/b". On Windows this
/// also turns "/" into "\".
fn tidy(path: &Path) -> PathBuf {
    path.components().collect()
}

impl Frecency {
    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    /// The score of `path` at `now`: 1 for one open just then, 0 for a file never opened.
    pub fn score(&self, path: &Path, now: u64) -> f64 {
        tidy(path)
            .to_str()
            .and_then(|path| self.at.get(path))
            .map_or(0.0, |&at| score(at, now))
    }

    /// Counts one open of `path` at `now`, then forgets the files whose score fell under 1/16
    /// and, past 1024 files, the weakest other than `path`. False for a path that is not
    /// UTF-8, which no index holds, or one longer than 4096 bytes.
    pub fn record(&mut self, path: &Path, now: u64) -> bool {
        let path = tidy(path);
        let Some(path) = path
            .to_str()
            .filter(|path| !path.is_empty() && path.len() <= LONGEST_PATH)
        else {
            return false;
        };
        let now = now as f64;
        for at in self.at.values_mut() {
            *at = healed(*at, now);
        }
        match self.at.get_mut(path) {
            Some(at) => *at = add_open(*at, now),
            None => {
                self.at.insert(path.into(), now);
            }
        }
        self.at.retain(|_, &mut at| at >= now - FORGET_AFTER);
        while self.at.len() > MOST_FILES {
            let weakest = self
                .at
                .iter()
                .filter(|(other, _)| ***other != *path)
                .min_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(weakest, _)| weakest.clone());
            if let Some(weakest) = weakest {
                self.at.remove(&weakest);
            }
        }
        true
    }

    /// Reads a list `save` wrote. A missing file is an empty list, as before the first open.
    pub fn open(file: &Path) -> Result<Self, FrecencyError> {
        let file = match File::open(file) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MOST_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        Self::decode(&bytes)
    }

    /// Written beside `file` and renamed over it, as `save_index` does, so a crash mid-save
    /// leaves the old list whole. Makes the folder first when it is missing.
    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(folder) = file
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty())
        {
            fs::create_dir_all(folder)?;
        }
        let _turn = SAVING.lock().unwrap_or_else(PoisonError::into_inner);
        let temp = file.with_added_extension(format!("{}.tmp", std::process::id()));
        let mut out = match File::create_new(&temp) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temp)?;
                File::create_new(&temp)?
            }
            out => out?,
        };
        let saved = out
            .write_all(&self.encode())
            .and_then(|()| out.sync_all())
            .and_then(|()| fs::rename(&temp, file));
        if saved.is_err() {
            let _ = fs::remove_file(&temp);
        }
        saved
    }

    fn encode(&self) -> Vec<u8> {
        let mut files: Vec<(&str, f64)> = self.at.iter().map(|(path, &at)| (&**path, at)).collect();
        files.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let bytes = files.iter().map(|(path, _)| 10 + path.len()).sum::<usize>();
        let mut out = Vec::with_capacity(HEADER + bytes);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, at) in files {
            out.extend_from_slice(&at.to_le_bytes());
            out.extend_from_slice(&(path.len() as u16).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
        }
        let crc = crc32fast::hash(&out);
        out[CRC].copy_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, FrecencyError> {
        let take =
            |at: usize, len: usize| bytes.get(at..at + len).ok_or(FrecencyError::WrongLength);
        let number = |at: usize| take(at, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        // Another kind of file, even a short one, is never taken for a list cut short and
        // saved over.
        if !MAGIC.starts_with(&bytes[..bytes.len().min(MAGIC.len())]) {
            return Err(FrecencyError::NotAList);
        }
        // Before the length, so a newer build's longer list is never taken for a damaged one.
        let version = number(8)?;
        if version != VERSION {
            return Err(FrecencyError::Version(version));
        }
        if bytes.len() < HEADER || bytes.len() > MOST_BYTES {
            return Err(FrecencyError::WrongLength);
        }
        let count = number(16)? as usize;
        if count > MOST_FILES {
            return Err(FrecencyError::Damaged);
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(&bytes[..CRC.start]);
        crc.update(&[0; 4]);
        crc.update(&bytes[CRC.end..]);
        let crc_holds = crc.finalize() == number(CRC.start)?;

        let mut at = HEADER;
        let mut list = Self {
            at: HashMap::with_capacity(count),
        };
        for _ in 0..count {
            let opened = f64::from_le_bytes(take(at, 8)?.try_into().unwrap());
            let len = usize::from(u16::from_le_bytes(take(at + 8, 2)?.try_into().unwrap()));
            let path = take(at + 10, len)?;
            at += 10 + len;
            let path = std::str::from_utf8(path).map_err(|_| FrecencyError::Damaged)?;
            let sound = opened.is_finite() && !path.is_empty() && len <= LONGEST_PATH;
            if !sound || list.at.insert(path.into(), opened).is_some() {
                return Err(FrecencyError::Damaged);
            }
        }
        if at != bytes.len() {
            return Err(FrecencyError::WrongLength);
        }
        if !crc_holds {
            return Err(FrecencyError::Damaged);
        }
        Ok(list)
    }

    /// Each file's boost at `now`, by its entry in `index`, and each app's, for
    /// `Searcher::search_with`. Reads every entry once, so work it out again only when the list,
    /// the index or the apps change.
    pub fn boosts(&self, index: &Index, apps: &Apps, now: u64) -> Boosts {
        let mut boosts = Boosts::default();
        if self.at.is_empty() {
            return boosts;
        }
        // Keyed by the bundle's path, so an app opened from its row or from its bundle in
        // ~/Applications shares one boost, and keeps it when the app row hides the bundle's.
        for app in 0..apps.len() as AppId {
            let path = tidy(&apps.path(app));
            if let Some(&at) = path.to_str().and_then(|path| self.at.get(path)) {
                let boost = boost(score(at, now));
                if boost > 0 {
                    boosts.apps.insert(app, boost);
                }
            }
        }
        if index.len() == 0 {
            return boosts;
        }
        let root = Path::new(index.name(0));
        // Each file below the root, by its own name: the folders on the way there, and its boost.
        let mut wanted: HashMap<&str, Vec<(Vec<&str>, u32)>> = HashMap::new();
        for (path, &at) in &self.at {
            let boost = boost(score(at, now));
            let Ok(below) = Path::new(&**path).strip_prefix(root) else {
                continue;
            };
            let names: Option<Vec<&str>> = below
                .components()
                .map(|name| match name {
                    Component::Normal(name) => name.to_str(),
                    _ => None,
                })
                .collect();
            if let Some((name, folders)) = names.as_deref().and_then(<[&str]>::split_last)
                && boost > 0
            {
                wanted
                    .entry(name)
                    .or_default()
                    .push((folders.to_vec(), boost));
            }
        }
        if wanted.is_empty() {
            return boosts;
        }

        // The letter fingerprint rules out most names before any is hashed.
        let fingerprints: HashSet<u64> =
            wanted.keys().map(|name| name_fingerprint(name).0).collect();
        let named: HashMap<NameId, &[(Vec<&str>, u32)]> = (0..index.distinct_names() as NameId)
            .into_par_iter()
            .filter(|&name| fingerprints.contains(&index.name_letters[name as usize]))
            .filter_map(|name| {
                let files = wanted.get(index.name_text(name))?;
                Some((name, files.as_slice()))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect();
        let mut is_named = vec![0_u64; index.distinct_names().div_ceil(64)];
        for &name in named.keys() {
            is_named[name as usize / 64] |= 1 << (name % 64);
        }
        let found: Vec<(EntryId, u32)> = index
            .entry_name
            .par_chunks(CHUNK)
            .enumerate()
            .flat_map_iter(|(chunk, names)| {
                let (is_named, named) = (&is_named, &named);
                let first = chunk * CHUNK;
                let runs = index.runs_in(first..first + names.len());
                let runs = runs.filter(|&(_, folder)| folder != NO_FOLDER);
                runs.flat_map(move |(entries, folder)| {
                    let parent = index.folders[folder as usize];
                    entries.filter_map(move |entry| {
                        let name = index.entry_name[entry];
                        if is_named[name as usize / 64] >> (name % 64) & 1 == 0 {
                            return None;
                        }
                        let (_, boost) = named[&name]
                            .iter()
                            .find(|(folders, _)| reached_through(index, parent, folders))?;
                        Some((entry as EntryId, *boost))
                    })
                })
            })
            .collect();
        boosts.entries.extend(found);
        boosts
    }
}

/// Whether climbing from `folder` meets `names`, last to first, then the root.
fn reached_through(index: &Index, mut folder: EntryId, names: &[&str]) -> bool {
    for &name in names.iter().rev() {
        if folder == 0 || folder == NO_PARENT || index.name(folder) != name {
            return false;
        }
        folder = index.parent(folder);
    }
    folder == 0
}

/// How much frecency adds to each entry's and app's score; most have none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Boosts {
    entries: HashMap<EntryId, u32>,
    apps: HashMap<AppId, u32>,
}

impl Boosts {
    pub fn get(&self, item: Item) -> u32 {
        let boost = match item {
            Item::Entry(entry) => self.entries.get(&entry),
            Item::App(app) => self.apps.get(&app),
        };
        boost.copied().unwrap_or(0)
    }

    /// The entries and apps boosted.
    pub fn len(&self) -> usize {
        self.entries.len() + self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.apps.is_empty()
    }

    /// The boosted entries; apps are boosted as they are matched.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (EntryId, u32)> + '_ {
        self.entries.iter().map(|(&entry, &boost)| (entry, boost))
    }

    #[cfg(test)]
    pub(crate) fn with_apps(mut self, apps: impl IntoIterator<Item = (AppId, u32)>) -> Self {
        self.apps.extend(apps);
        self
    }
}

#[cfg(test)]
impl FromIterator<(EntryId, u32)> for Boosts {
    fn from_iter<I: IntoIterator<Item = (EntryId, u32)>>(boosts: I) -> Self {
        Self {
            entries: boosts.into_iter().collect(),
            apps: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexBuilder;

    const DAY: u64 = 24 * 60 * 60;
    const NOW: u64 = 1_790_000_000;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    fn random(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut seed = seed;
        move |below| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        }
    }

    fn opened(files: &[(&str, &[u64])]) -> Frecency {
        let mut list = Frecency::default();
        for &(path, times) in files {
            for &time in times {
                assert!(list.record(Path::new(path), time));
            }
        }
        list
    }

    #[test]
    fn a_score_halves_every_30_days() {
        let plan = Path::new("/home/plan.md");
        let mut list = opened(&[("/home/plan.md", &[NOW])]);
        assert_eq!(list.score(plan, NOW), 1.0);
        assert!(close(list.score(plan, NOW + 30 * DAY), 0.5));
        assert!(close(list.score(plan, NOW + 60 * DAY), 0.25));
        assert_eq!(list.score(Path::new("/home/other.md"), NOW), 0.0);
        list.record(plan, NOW);
        assert!(close(list.score(plan, NOW), 2.0));
        let month_apart = opened(&[("/home/plan.md", &[NOW - 30 * DAY, NOW])]);
        assert!(close(month_apart.score(plan, NOW), 1.5));
    }

    /// One number per file scores the same as adding up every open, each halved once per 30
    /// days since, whatever order the opens are counted in.
    #[test]
    fn one_number_adds_up_every_open_in_any_order() {
        let mut next = random(0x9E37_79B9_7F4A_7C15);
        let path = Path::new("/home/a");
        for pass in 0..300 {
            let times: Vec<u64> = (0..=next(20)).map(|_| NOW - next(100 * DAY)).collect();
            let now = NOW + next(10 * DAY);
            let expected: f64 = times
                .iter()
                .map(|&time| ((time as f64 - now as f64) / HALF_LIFE).exp2())
                .sum();
            let forward = opened(&[("/home/a", &times)]);
            let mut backward = times.clone();
            backward.reverse();
            let backward = opened(&[("/home/a", &backward)]);
            for list in [forward, backward] {
                let score = list.score(path, now);
                assert!(close(score, expected), "pass {pass}: {score} {expected}");
            }
        }
    }

    #[test]
    fn the_file_with_the_later_number_scores_higher_at_every_moment() {
        let mut next = random(0x2545_F491_4F6C_DD1D);
        for _ in 0..300 {
            let a: Vec<u64> = (0..=next(8)).map(|_| NOW - next(90 * DAY)).collect();
            let b: Vec<u64> = (0..=next(8)).map(|_| NOW - next(90 * DAY)).collect();
            let list = opened(&[("/a", &a), ("/b", &b)]);
            let higher = list.at[&*key("/a")] > list.at[&*key("/b")];
            for later in [0, DAY, 45 * DAY, 400 * DAY] {
                let now = NOW + later;
                let (a, b) = (Path::new("/a"), Path::new("/b"));
                assert_eq!(list.score(a, now) > list.score(b, now), higher);
            }
        }
    }

    #[test]
    fn a_boost_grows_by_a_letter_per_doubling_up_to_four() {
        assert_eq!(boost(0.0), 0);
        // Under 1/16 a file is forgotten, though rounding would still give it a point.
        assert_eq!(boost(0.035), 0);
        assert_eq!(boost(1.0 / 16.0), 1);
        assert_eq!(boost(1.0), 12);
        assert_eq!(boost(3.0), 24);
        assert_eq!(boost(7.0), 36);
        assert_eq!(boost(15.0), 48);
        assert_eq!(boost(1e300), 48);
        assert_eq!(boost(f64::INFINITY), 48);
        let mut last = 0;
        for step in 0..10_000 {
            let now = boost(f64::from(step) / 100.0);
            assert!(now >= last, "{step}");
            last = now;
        }
    }

    #[test]
    fn forgets_a_file_opened_once_four_months_ago() {
        let old = Path::new("/home/old.md");
        let mut list = opened(&[("/home/old.md", &[NOW])]);
        list.record(Path::new("/home/new.md"), NOW + 120 * DAY);
        assert_eq!(list.score(old, NOW + 120 * DAY), 1.0 / 16.0);
        list.record(Path::new("/home/new.md"), NOW + 120 * DAY + 1);
        assert_eq!(list.score(old, NOW), 0.0);
        assert_eq!(list.len(), 1);
    }

    /// A folder may come with a slash at the end, as a file URL's path keeps one.
    #[test]
    fn spellings_of_one_path_count_as_one_file() {
        let mut list = Frecency::default();
        for spelling in ["/home/proj", "/home/proj/", "/home//proj", "/home/./proj"] {
            assert!(list.record(Path::new(spelling), NOW));
        }
        assert_eq!(list.len(), 1);
        assert!(close(list.score(Path::new("/home/proj/"), NOW), 4.0));
        assert!(!list.record(Path::new(""), NOW));
    }

    #[test]
    fn keeps_1024_files_and_drops_the_weakest_but_never_the_one_just_opened() {
        let mut list = Frecency::default();
        for file in 0..MOST_FILES as u64 {
            let path = format!("/home/{file}");
            // File 0 is the weakest, opened earliest.
            for _ in 0..3 {
                list.record(Path::new(&path), NOW + file);
            }
        }
        assert_eq!(list.len(), MOST_FILES);
        let new = Path::new("/home/new");
        list.record(new, NOW + MOST_FILES as u64);
        assert_eq!(list.len(), MOST_FILES);
        assert_eq!(list.score(new, NOW + MOST_FILES as u64), 1.0);
        assert_eq!(list.score(Path::new("/home/0"), NOW), 0.0);
        assert!(list.score(Path::new("/home/1"), NOW) > 2.9);
    }

    #[test]
    fn at_the_limit_a_tie_drops_the_same_file_whatever_the_order() {
        let mut next = random(0xA076_1D64_78BD_642F);
        for _ in 0..5 {
            let mut paths: Vec<String> = (0..MOST_FILES).map(|f| format!("/home/{f:04}")).collect();
            for i in (1..paths.len()).rev() {
                paths.swap(i, next(i as u64 + 1) as usize);
            }
            let mut list = Frecency::default();
            for path in &paths {
                list.record(Path::new(path), NOW);
            }
            list.record(Path::new("/home/new"), NOW);
            assert_eq!(list.len(), MOST_FILES);
            assert!(!list.at.contains_key(&*key("/home/1023")));
            assert!(list.at.contains_key(&*key("/home/new")));
        }
    }

    #[test]
    fn a_clock_that_ran_ahead_counts_as_one_open_now() {
        let ahead = Path::new("/home/ahead.md");
        let mut list = opened(&[("/home/ahead.md", &[NOW + 3650 * DAY])]);
        assert_eq!(list.score(ahead, NOW), 1.0);
        list.record(Path::new("/home/today.md"), NOW);
        assert_eq!(list.at[&*key("/home/ahead.md")], NOW as f64);
        list.record(Path::new("/home/today.md"), NOW);
        assert!(list.score(ahead, NOW) < list.score(Path::new("/home/today.md"), NOW));

        let mut bytes = opened(&[("/ab", &[NOW])]).encode();
        bytes[HEADER..HEADER + 8].copy_from_slice(&1e300_f64.to_le_bytes());
        fix_crc(&mut bytes);
        let saved = Frecency::decode(&bytes).unwrap();
        assert_eq!(saved.score(Path::new("/ab"), NOW), 1.0);
        let index = {
            let mut builder = IndexBuilder::new();
            let root = builder.push("/", NO_PARENT, true);
            builder.push("ab", root, false);
            builder.finish()
        };
        let boosts = boosts_of(&saved, &index, NOW);
        assert_eq!(boosts.get(Item::Entry(1)), boost(1.0));

        // A file opened a thousand times today is still believed.
        let busy = opened(&[("/busy", &[NOW; 1000])]);
        assert!(close(busy.score(Path::new("/busy"), NOW), 1000.0));
    }

    /// A file opened all day, every day, stops at 1024 rather than passing for a clock that ran
    /// ahead, even with the clock set back a little.
    #[test]
    fn a_file_opened_all_the_time_stays_at_the_most() {
        let busy = Path::new("/home/busy.md");
        let mut list = opened(&[("/home/busy.md", &[NOW; 3000])]);
        assert!(close(list.score(busy, NOW), 1024.0));
        list.record(Path::new("/home/other.md"), NOW - 20 * DAY);
        assert!(list.score(busy, NOW - 20 * DAY) > 1024.0);
        list.record(busy, NOW);
        assert!(close(list.score(busy, NOW), 1024.0));
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("busy.md", root, false);
        let index = builder.finish();
        assert_eq!(boosts_of(&list, &index, NOW).get(Item::Entry(1)), 48);
    }

    /// After every open, with the clock going back now and then: at most 1024 files, each
    /// within four months of that open and none far ahead of it, and the file just opened kept.
    #[test]
    fn every_open_leaves_a_sound_list() {
        let mut next = random(0xBF58_476D_1CE4_E5B9);
        let mut list = Frecency::default();
        let mut now = NOW;
        for _ in 0..6_000 {
            now = match next(20) {
                0 => now - next(200 * DAY),
                1 => now + next(200 * DAY),
                _ => now + next(DAY),
            };
            let path = format!("/home/{}", next(1_300));
            assert!(list.record(Path::new(&path), now));
            let now = now as f64;
            assert!(list.len() <= MOST_FILES);
            assert!(list.at.contains_key(&*key(&path)));
            for &at in list.at.values() {
                assert!(at.is_finite() && at >= now - FORGET_AFTER, "{at} {now}");
                assert!(at <= now + MOST_AHEAD + CLOCK_SLACK, "{at} {now}");
            }
        }
    }

    #[test]
    fn refuses_paths_no_index_holds() {
        let mut list = Frecency::default();
        assert!(!list.record(Path::new(""), NOW));
        let longest = format!("/{}", "a".repeat(LONGEST_PATH - 1));
        assert!(list.record(Path::new(&longest), NOW));
        assert!(!list.record(Path::new(&format!("{longest}a")), NOW));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let broken = std::ffi::OsStr::from_bytes(b"/home/\xFF");
            assert!(!list.record(Path::new(broken), NOW));
        }
        assert_eq!(list.len(), 1);
    }

    fn sample() -> Frecency {
        opened(&[
            ("/home/plan.md", &[NOW - 3 * DAY, NOW]),
            ("/home/café/naïve résumé.pdf", &[NOW - 40 * DAY]),
            ("/home/会議", &[NOW, NOW, NOW]),
        ])
    }

    fn fix_crc(bytes: &mut [u8]) {
        bytes[CRC].fill(0);
        let crc = crc32fast::hash(bytes);
        bytes[CRC].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn opens_what_it_saved_making_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("io.example/frecency");
        let list = sample();
        list.save(&file).unwrap();
        assert_eq!(Frecency::open(&file).unwrap(), list);
        let missing = Frecency::open(&dir.path().join("missing")).unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn opens_what_it_saved_for_many_made_up_lists() {
        let mut next = random(0xD1B5_4A32_D192_ED03);
        let letters = ["a", "/", "é", ".", " ", "会議", "\u{0}", "Z"];
        for pass in 0..200 {
            let mut list = Frecency::default();
            for _ in 0..next(40) {
                let len = 1 + next(12) as usize;
                let path: String = (0..len)
                    .map(|_| letters[next(letters.len() as u64) as usize])
                    .collect();
                for _ in 0..=next(3) {
                    list.record(Path::new(&path), NOW - next(100 * DAY));
                }
            }
            let decoded = Frecency::decode(&list.encode()).unwrap();
            assert_eq!(decoded, list, "pass {pass}");
        }
    }

    #[test]
    fn the_same_list_saves_the_same_bytes() {
        let a = opened(&[("/b", &[NOW]), ("/a", &[NOW - DAY])]);
        let b = opened(&[("/a", &[NOW - DAY]), ("/b", &[NOW])]);
        assert_eq!(a.encode(), b.encode());
    }

    #[test]
    fn numbers_are_little_endian_at_fixed_places() {
        let bytes = opened(&[("/b", &[2]), ("/a", &[1])]).encode();
        assert_eq!(bytes[..8], *b"TL-OPENS");
        assert_eq!(bytes[8..12], [1, 0, 0, 0]);
        let mut zeroed = bytes.clone();
        zeroed[12..16].fill(0);
        assert_eq!(bytes[12..16], crc32fast::hash(&zeroed).to_le_bytes());
        assert_eq!(bytes[16..20], [2, 0, 0, 0]);
        assert_eq!(bytes[20..28], 1.0_f64.to_le_bytes());
        assert_eq!(bytes[28..32], [&[2, 0], key("/a").as_bytes()].concat());
        assert_eq!(bytes[32..40], 2.0_f64.to_le_bytes());
        assert_eq!(bytes[40..], [&[2, 0], key("/b").as_bytes()].concat());
    }

    #[test]
    fn refuses_a_list_cut_short_or_too_long() {
        let bytes = sample().encode();
        for length in 0..bytes.len() {
            let decoded = Frecency::decode(&bytes[..length]);
            assert!(
                matches!(decoded, Err(FrecencyError::WrongLength)),
                "length {length}: {decoded:?}"
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        fix_crc(&mut longer);
        assert!(matches!(
            Frecency::decode(&longer),
            Err(FrecencyError::WrongLength)
        ));
    }

    #[test]
    fn refuses_any_flipped_bit() {
        let bytes = sample().encode();
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1 << bit;
                let decoded = Frecency::decode(&damaged);
                let expected = match at {
                    0..8 => matches!(decoded, Err(FrecencyError::NotAList)),
                    8..12 => matches!(decoded, Err(FrecencyError::Version(_))),
                    12..16 => matches!(decoded, Err(FrecencyError::Damaged)),
                    _ => decoded.is_err(),
                };
                assert!(expected, "byte {at} bit {bit}: {decoded:?}");
            }
        }
    }

    /// Damage the CRC cannot catch, as a bug in saving would leave it.
    #[test]
    fn refuses_unsound_files_with_a_good_crc() {
        let one = opened(&[("/ab", &[NOW])]).encode();
        let unsound: [fn(&mut Vec<u8>); 6] = [
            |bytes| bytes[HEADER + 11] = 0xFF,
            |bytes| bytes[HEADER..HEADER + 8].copy_from_slice(&f64::NAN.to_le_bytes()),
            |bytes| bytes[HEADER..HEADER + 8].copy_from_slice(&f64::INFINITY.to_le_bytes()),
            |bytes| {
                let file = bytes[HEADER..].to_vec();
                bytes.extend_from_slice(&file);
                bytes[16] = 2;
            },
            |bytes| {
                bytes.truncate(HEADER + 10);
                bytes[HEADER + 8] = 0;
            },
            |bytes| bytes[16..20].copy_from_slice(&(MOST_FILES as u32 + 1).to_le_bytes()),
        ];
        for (case, damage) in unsound.iter().enumerate() {
            let mut bytes = one.clone();
            damage(&mut bytes);
            fix_crc(&mut bytes);
            let decoded = Frecency::decode(&bytes);
            assert!(
                matches!(decoded, Err(FrecencyError::Damaged)),
                "case {case}: {decoded:?}"
            );
        }
        assert!(Frecency::decode(&one).is_ok());
    }

    #[test]
    fn refuses_other_files_and_a_huge_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let opened = |bytes: &[u8]| {
            fs::write(&file, bytes).unwrap();
            Frecency::open(&file)
        };
        assert!(matches!(
            opened(b"bplist00 and more than twenty bytes"),
            Err(FrecencyError::NotAList)
        ));
        assert!(matches!(opened(b"notes"), Err(FrecencyError::NotAList)));
        assert!(matches!(opened(b""), Err(FrecencyError::WrongLength)));
        assert!(matches!(opened(b"TL-OP"), Err(FrecencyError::WrongLength)));
        let mut bytes = sample().encode();
        bytes[8] = 2;
        assert!(matches!(opened(&bytes), Err(FrecencyError::Version(2))));
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&VERSION.to_le_bytes());
        huge.resize(MOST_BYTES + 1, 0);
        assert!(matches!(opened(&huge), Err(FrecencyError::WrongLength)));
        // A newer build's list may be longer, and is still left as it is.
        huge[8] = 2;
        assert!(matches!(opened(&huge), Err(FrecencyError::Version(2))));
        huge[0] = b'x';
        assert!(matches!(opened(&huge), Err(FrecencyError::NotAList)));
        assert!(matches!(
            Frecency::open(dir.path()),
            Err(FrecencyError::Io(_))
        ));
    }

    /// Only a list of ours, cut short or damaged, may be saved over.
    #[test]
    fn only_a_damaged_list_is_given_up() {
        let damaged = [FrecencyError::WrongLength, FrecencyError::Damaged];
        assert!(damaged.iter().all(FrecencyError::is_damaged));
        let kept = [
            FrecencyError::NotAList,
            FrecencyError::Version(2),
            FrecencyError::Io(io::ErrorKind::PermissionDenied.into()),
        ];
        assert!(!kept.iter().any(FrecencyError::is_damaged));
    }

    #[test]
    fn a_temporary_file_left_by_a_crash_does_not_stop_saving() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let temp = file.with_added_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temp, "half a list").unwrap();
        let list = sample();
        list.save(&file).unwrap();
        assert_eq!(Frecency::open(&file).unwrap(), list);
        assert!(!temp.exists());
    }

    #[test]
    fn replaces_the_old_file_and_leaves_no_temporary_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        sample().save(&file).unwrap();
        let newer = opened(&[("/home/new.md", &[NOW])]);
        newer.save(&file).unwrap();
        assert_eq!(Frecency::open(&file).unwrap(), newer);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        // A folder where the file should be: the save fails late, at the rename.
        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("inside"), "mine").unwrap();
        assert!(newer.save(&folder).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        assert_eq!(fs::read_to_string(folder.join("inside")).unwrap(), "mine");
    }

    fn tree() -> Index {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let docs = builder.push("docs", root, true);
        builder.push("README.md", docs, false);
        let src = builder.push("src", root, true);
        builder.push("README.md", src, false);
        builder.push("README.md", root, false);
        builder.finish()
    }

    /// The path as the list keeps it, with "\" for "/" on Windows.
    fn key(path: &str) -> String {
        tidy(Path::new(path)).to_str().unwrap().to_owned()
    }

    fn boosts_of(list: &Frecency, index: &Index, now: u64) -> Boosts {
        list.boosts(index, &Apps::default(), now)
    }

    fn entry(index: &Index, path: &str) -> EntryId {
        index
            .ids()
            .find(|&entry| index.path(entry) == Path::new(path))
            .unwrap()
    }

    #[test]
    fn boosts_reach_each_file_and_folder_opened_and_nothing_else() {
        let index = tree();
        let list = opened(&[
            ("/home/docs/README.md", &[NOW, NOW]),
            ("/home/src", &[NOW]),
            ("/home/README.md", &[NOW - 120 * DAY]),
            ("/home/missing.md", &[NOW]),
            ("/home/docs/readme.md", &[NOW]),
            ("/home/docs/README.md/inside", &[NOW]),
            ("/elsewhere/README.md", &[NOW]),
            ("/home", &[NOW]),
            ("/home/../home/src", &[NOW]),
        ]);
        let boosts = boosts_of(&list, &index, NOW);
        let expected: Boosts = [
            (entry(&index, "/home/docs/README.md"), boost(2.0)),
            (entry(&index, "/home/src"), 12),
            (entry(&index, "/home/README.md"), 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(boosts, expected);
        let unopened = entry(&index, "/home/src/README.md");
        assert_eq!(boosts.get(Item::Entry(unopened)), 0);
        assert!(boosts_of(&Frecency::default(), &index, NOW).is_empty());
        assert!(boosts_of(&list, &IndexBuilder::new().finish(), NOW).is_empty());
    }

    /// An app opened from its row or from its bundle in the index counts as one, by the
    /// bundle's path, wherever the app sits.
    #[test]
    fn an_app_is_boosted_by_its_bundle_path() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let applications = builder.push("Applications", root, true);
        let bundle = builder.push("Gmail.app", applications, true);
        let index = builder.finish();
        let mut apps = Apps::from_names(Path::new("/home/Applications"), &["Gmail", "Notes"]);
        let list = opened(&[("/home/Applications/Gmail.app", &[NOW, NOW])]);
        let boosts = list.boosts(&index, &apps, NOW);
        assert_eq!(boosts.get(Item::App(0)), boost(2.0));
        assert_eq!(boosts.get(Item::Entry(bundle)), boost(2.0));
        assert_eq!(boosts.get(Item::App(1)), 0);
        assert_eq!(boosts.len(), 2);

        // Outside the indexed folder, like /Applications, only the app row has it.
        apps = Apps::from_names(Path::new("/Applications"), &["Notes", "Safari"]);
        let list = opened(&[("/Applications/Safari.app", &[NOW])]);
        let boosts = list.boosts(&index, &apps, NOW);
        assert_eq!(boosts.get(Item::App(1)), 12);
        assert_eq!(boosts.len(), 1);
        let empty = IndexBuilder::new().finish();
        assert_eq!(list.boosts(&empty, &apps, NOW), boosts);
    }

    /// A list is pruned only when a file is opened, so one left alone still holds files whose
    /// score fell under 1/16, and they must boost nothing.
    #[test]
    fn a_list_left_alone_boosts_nothing_it_has_forgotten() {
        let index = tree();
        let list = opened(&[("/home/docs/README.md", &[NOW, NOW, NOW])]);
        // Three opens fall to 3/32 after five months and to 3/64 after six.
        assert_eq!(boosts_of(&list, &index, NOW + 150 * DAY).len(), 1);
        assert!(boosts_of(&list, &index, NOW + 180 * DAY).is_empty());
        assert!(boosts_of(&list, &index, NOW + 365 * DAY).is_empty());
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn a_removed_entry_gets_no_boost() {
        let mut index = tree();
        let readme = entry(&index, "/home/docs/README.md");
        let list = opened(&[("/home/docs/README.md", &[NOW])]);
        assert_eq!(boosts_of(&list, &index, NOW).len(), 1);
        let mut parents = index.parents();
        parents[readme as usize] = NO_PARENT;
        index.siblings = crate::siblings::Siblings::from_parents(&parents, &index.folders);
        assert!(boosts_of(&list, &index, NOW).is_empty());
    }

    #[test]
    fn boosts_match_a_path_by_path_lookup_on_made_up_names() {
        let index = crate::stand_in(20_000);
        let mut next = random(0x1234_5678_9ABC_DEF1);
        let mut list = Frecency::default();
        for _ in 0..500 {
            let path = index.path(next(index.len() as u64) as EntryId);
            for _ in 0..=next(4) {
                list.record(&path, NOW - next(60 * DAY));
            }
            list.record(&path.join("not there"), NOW);
        }
        let boosts = boosts_of(&list, &index, NOW);
        let expected: Boosts = index
            .ids()
            .skip(1)
            .map(|entry| (entry, boost(list.score(&index.path(entry), NOW))))
            .filter(|&(_, boost)| boost > 0)
            .collect();
        assert!(boosts.len() > 400, "{}", boosts.len());
        assert_eq!(boosts, expected);
    }
}
