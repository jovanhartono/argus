//! The hatch: the few C-style functions a window calls to reach the engine. The macOS
//! window calls them from Swift, and the Windows window will reuse them at M5.
//!
//! The window calls in from its main thread and never waits for a search. `tl_submit` hands
//! the search to a worker and returns its ticket at once; the worker calls `on_event` when
//! that ticket's rows are ready; the window then copies them out with `tl_take`.
//! When files change, `tl_changed` asks a second thread to read the changed folders again into
//! the list in use, while new searches wait, or to build a new list beside it when macOS lost
//! track; a new list reaches the worker through the same queue as the searches, so each search
//! sees one whole list. `tl_volume_added` and `tl_volume_removed` go through that thread too,
//! without reading the home folder again: a drive joins a copy of the newest list, as reading
//! one takes seconds, and one taken out leaves the list in use.
//! Given a file, a core opens the list saved there instead of scanning, and a third thread
//! saves each new list to it once files stop changing for a moment.
//! A text search, inside files, runs the same way on a worker of its own, started with the
//! first one, so a name search never waits behind it: `tl_grep_submit`, then `tl_grep_take`
//! for each batch of lines found.
//! `include/argus.h` repeats everything here for Swift, and a test keeps the two in step.

use argus_core::{
    Access, Apps, Boosts, Clutter, Frecency, Hit, Ignore, Index, Item, OpenError, Pages, Refusal,
    Scope, Searcher, Settings, Ticket, Watch, Within, open_index, remove_old_temps, save_index,
    unix_now,
};
use argus_grep::{Found, Grep, Report};
use std::ffi::{CString, c_void};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, TryLockError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[global_allocator]
static PAGES: Pages = Pages;

/// Raise this with any change below, so a window built against an older header refuses to start.
pub const TL_ABI_VERSION: u32 = 12;

pub const TL_OK: u32 = 0;
/// No rows for that ticket: not ready yet, already taken, or a newer search replaced them.
pub const TL_GONE: u32 = 1;
pub const TL_PANICKED: u32 = 2;

/// The index is built; `value` is its entry count.
pub const TL_EVENT_INDEXED: u32 = 1;
/// `value` is the ticket whose rows are ready.
pub const TL_EVENT_READY: u32 = 2;
/// The folder couldn't be scanned, or the worker hit a bug; no more results will come.
/// `value` is the scan's OS error number (errno on a Mac), or 0.
pub const TL_EVENT_FAILED: u32 = 3;
/// A problem with your ignore list or the list of opened files. `value` is the address of its
/// text, NUL-terminated UTF-8, readable only until `on_event` returns.
pub const TL_EVENT_NOTE: u32 = 4;
/// A list built again, or patched, after a change is in use. `value` holds its entry count in
/// the low 32 bits and the milliseconds the rebuild took in the high 32.
pub const TL_EVENT_REBUILT: u32 = 5;
/// `value` is the text search ticket whose lines, or whose end, wait for `tl_grep_take`. One
/// per take: no more come until the window has taken what was waiting.
pub const TL_EVENT_LINES: u32 = 6;
/// The saved list is in use, in place of a first scan; `value` is its entry count.
pub const TL_EVENT_OPENED: u32 = 7;
/// The same search may now answer in another order: an open was counted, or the apps read
/// again differ. `value` is 0.
pub const TL_EVENT_RERANKED: u32 = 8;

const ROWS: usize = 50;
/// Lines a text search shows, across all its folders.
const LINES: usize = 100;

/// Rebuilds start at least this far apart while files keep changing, so a long download
/// doesn't keep a core busy. A change is findable within FSEvents' 0.2 s, plus this, plus
/// one rebuild: about 0.15 s for a home folder.
const GAP: Duration = Duration::from_millis(500);

/// A burst of changes writes the saved list once, after files stay still this long, or at
/// the latest `SAVE_WITHIN` after its first new list.
const SAVE_AFTER: Duration = Duration::from_secs(2);
const SAVE_WITHIN: Duration = Duration::from_secs(60);
/// Edits that change no names bring no new list, so the saved one is written again this
/// often at most, only so the next launch has less to replay.
const RESAVE: Duration = Duration::from_secs(60 * 60);
/// No save takes this long, so a temporary file this old was left by a quit mid-save.
const LEFT_FOR: Duration = Duration::from_secs(60 * 60);

/// Scores fade about 2% a day, so boosts worked out an hour ago are still right.
const RERANK: Duration = Duration::from_secs(60 * 60);

/// Runs on Rust threads, at times several at once, never the window's main thread, and must
/// never wait for it.
pub type TlEventFn = unsafe extern "C" fn(ctx: *mut c_void, event: u32, value: u64);

/// One result. Its name and folder are byte ranges in `TlRows::text`. An app from `tl_apps` is
/// named without ".app", as Finder shows it, and opens at `<folder>/<name>.app`.
#[repr(C)]
pub struct TlRow {
    pub name_start: u32,
    pub name_len: u32,
    pub folder_start: u32,
    pub folder_len: u32,
    pub is_folder: bool,
    pub is_app: bool,
}

/// The window copies what it needs, then hands this back to `tl_rows_free` once.
#[repr(C)]
pub struct TlRows {
    pub rows: *const TlRow,
    pub count: usize,
    pub text: *const u8,
    owner: *mut c_void,
}

#[derive(Default)]
struct Packed {
    rows: Vec<TlRow>,
    text: String,
}

/// Where a core keeps its list between launches. `file` and `journal` are UTF-8; `journal` is
/// macOS's name for the FSEvents log of the scanned folder's volume, as a list saved under
/// another log can't catch up, and `now` the newest FSEvents event id as the core starts.
#[repr(C)]
pub struct TlSaved {
    pub file: *const u8,
    pub file_len: usize,
    pub journal: *const u8,
    pub journal_len: usize,
    pub now: u64,
}

/// One line a text search found. The file's path and the line are byte ranges in
/// `TlLines::text`; the match is a byte range in the line, counted from its start.
#[repr(C)]
pub struct TlLine {
    pub path_start: u32,
    pub path_len: u32,
    pub text_start: u32,
    pub text_len: u32,
    pub match_start: u32,
    pub match_len: u32,
    pub line: u64,
}

/// What a text search did with each file it met, as `argus_grep::Report` explains.
#[repr(C)]
pub struct TlReport {
    pub files_searched: u64,
    pub lines_found: u64,
    pub binary: u64,
    pub too_large: u64,
    pub cloud_only: u64,
    pub unreadable: u64,
}

impl From<Report> for TlReport {
    fn from(report: Report) -> Self {
        Self {
            files_searched: report.files_searched as u64,
            lines_found: report.lines_found as u64,
            binary: report.binary as u64,
            too_large: report.too_large as u64,
            cloud_only: report.cloud_only as u64,
            unreadable: report.unreadable as u64,
        }
    }
}

/// The lines found since the last take. Once `done`, the search is over and `report` holds its
/// counts; before, `report` is all zeros. `failed` comes with `done` when a bug in the engine
/// ended the search early: then `report` counts only the lines found. Hand this back to
/// `tl_lines_free` once.
#[repr(C)]
pub struct TlLines {
    pub lines: *const TlLine,
    pub count: usize,
    pub text: *const u8,
    pub done: bool,
    pub failed: bool,
    pub report: TlReport,
    owner: *mut c_void,
}

/// How the list in use is doing, for the window's health view. `entries` is 0 before the first
/// list. `scanned_at` is when the newest list read from the whole folder came into use, and
/// `scan_ms` how long reading it took; `scanned_at` is 0 while the list in use is the one saved
/// at the last launch, patched or not. `changed_at` is when the newest list after the first came
/// into use, or 0 for none yet. Times are Unix seconds. `busy` is set while a new list is being
/// built or waits to be. `building` is set only while that list reads the whole folder, as for
/// the first list, a Rebuild or the switch, not while changed folders are read again. `behind`
/// is set once a rebuild failed, so the list misses a change until a whole new list reads it.
/// `stopped` is set once the core stopped, as after `TL_EVENT_FAILED`, and no list will come.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct TlHealth {
    pub entries: u64,
    pub scanned_at: u64,
    pub scan_ms: u64,
    pub changed_at: u64,
    pub busy: bool,
    pub building: bool,
    pub behind: bool,
    pub stopped: bool,
}

#[derive(Default)]
struct Batch {
    lines: Vec<TlLine>,
    text: String,
}

/// The newest finished search, waiting for the window to take it.
type Done = Mutex<Option<(Ticket, Packed)>>;

pub struct TlCore {
    searcher: Arc<Searcher>,
    /// Emptied when the core is freed, which tells the worker to stop.
    jobs: Option<Sender<Job>>,
    done: Arc<Done>,
    live: Arc<Live>,
    worker: Option<JoinHandle<()>>,
    /// Only for a core given a file to save to.
    saver: Option<JoinHandle<()>>,
    saved_in: Option<PathBuf>,
    /// None only for a core a test built without threads.
    text: Option<Text>,
    /// Which volumes join the list, by their real paths; none for made-up names.
    judge: Option<Judge>,
}

/// The files opened from the window, for ranking, and the file they are kept in, if any.
#[derive(Default)]
struct Opens {
    frecency: Frecency,
    file: Option<PathBuf>,
}

impl TlCore {
    fn send(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }
}

type Judge = fn(&Path) -> Result<PathBuf, Refusal>;

/// What text search needs. Its threads start with the first search, so a window that never
/// searches inside files never pays for them.
struct Text {
    /// As the core was given it, for reading the ignore list.
    home: PathBuf,
    full_disk: AtomicBool,
    notify: Notify,
    started: OnceLock<TextSearch>,
}

struct TextSearch {
    grep: Arc<Grep>,
    jobs: Sender<Find>,
    waiting: Arc<Mutex<Waiting>>,
    worker: JoinHandle<()>,
}

struct Find {
    ticket: Ticket,
    folders: Vec<PathBuf>,
    query: String,
    access: Access,
}

/// What the newest text search found since the window last took some.
#[derive(Default)]
struct Waiting {
    ticket: u64,
    batch: Batch,
    /// Lines found so far, taken or not.
    found: usize,
    /// Set once the search is over.
    report: Option<Report>,
    failed: bool,
    /// Something is waiting, and `TL_EVENT_LINES` said so.
    told: bool,
}

enum Job {
    Search {
        ticket: Ticket,
        query: String,
        /// Only what is inside this folder, a full path.
        folder: Option<String>,
    },
    /// A list built again, or the one in use patched, and the milliseconds that took.
    Swap {
        list: List,
        took: u32,
    },
    Opened,
    /// The folders to read apps from, as `Apps::scan_folders` reads them.
    Apps(Vec<PathBuf>),
    /// Told from the worker, since `on_event` must never run on the window's main thread.
    Note(String),
}

/// What the rebuild thread waits on.
#[derive(Default)]
struct Plan {
    /// Something changed since the last rebuild started.
    wanted: bool,
    /// A rebuild is under way, so the newest list may already be out of date.
    running: bool,
    /// The rebuild under way reads the whole folder rather than patching the newest list.
    whole: bool,
    /// The list of the rebuild under way is in use; only its checks of late changes remain.
    listed: bool,
    full_disk: bool,
    /// The window's mark on the first change still waiting for a rebuild.
    asked: Option<u64>,
    /// Folders changed while a rebuild ran, judged by its list once it is in: otherwise, on a
    /// busy Mac, each save landing during a slow rebuild would ask for one more, forever.
    recheck: Option<(Vec<PathBuf>, u64)>,
    stopped: bool,
    /// Clutter is kept from the next list on.
    everything: bool,
    /// Clutter is kept in the newest list, or the one being built.
    everything_listed: bool,
    /// The mark of the switch that `everything` waits on, if any.
    switched: Option<u64>,
    /// The volumes the window said were mounted, by their real paths. Each list reads them
    /// once Full Disk Access is on.
    volumes: Vec<PathBuf>,
    /// Full Disk Access is on, so lists read `volumes`. Kept here rather than read beside
    /// the plan, so a volume added just as access is granted is never left out.
    granted: bool,
    /// `volumes` changed, or folders the newest list holds did, since the newest list, or
    /// the one being built, was started.
    patching: bool,
    /// Folders the newest list holds that changed, read again in place of a new list.
    changes: Vec<PathBuf>,
    /// The window's mark on the first such change still waiting.
    patched: Option<u64>,
    /// The newest FSEvents event id whose change is in the plan, or needed nothing, so a
    /// list started now is current to it.
    delivered: u64,
    /// The mark of the first change a failed rebuild was to read. Lists from then on miss
    /// it, so none is saved, and the next change builds the whole list again.
    failed: Option<u64>,
}

impl Plan {
    fn new(everything: bool, granted: bool) -> Self {
        Self {
            everything,
            everything_listed: everything,
            granted,
            ..Self::default()
        }
    }

    fn want(&mut self, mark: u64) {
        self.wanted = true;
        self.asked = Some(self.asked.map_or(mark, |asked| asked.min(mark)));
    }

    fn heard(&mut self, mark: u64) {
        self.delivered = self.delivered.max(mark);
    }

    /// Nothing waits or runs, so the newest list is current to `delivered`.
    fn idle(&self) -> bool {
        !(self.wanted || self.running || self.patching)
            && self.everything == self.everything_listed
            && self.recheck.is_none()
            && self.failed.is_none()
    }

    /// A new list is being built or waits to be. Unlike `idle`, a rebuild whose list is in
    /// use counts as done, as the window hears of that list before the rebuild ends.
    fn busy(&self) -> bool {
        self.wanted
            || (self.running && !self.listed)
            || self.patching
            || self.everything != self.everything_listed
    }

    /// As `busy`, but only for a list read from the whole folder: with everything listed,
    /// ~/Library changes every few seconds, and a spinner for each patch would never rest.
    fn building(&self) -> bool {
        self.wanted
            || (self.running && self.whole && !self.listed)
            || self.everything != self.everything_listed
    }

    /// The setting for the list about to be built, which brings any switch waiting.
    fn list(&mut self) -> Clutter {
        self.everything_listed = self.everything;
        self.switched = None;
        if self.everything {
            Clutter::Keep
        } else {
            Clutter::Skip
        }
    }

    /// The volumes for the list about to be built, and the folders to read again, which
    /// brings any patch waiting.
    fn volumes(&mut self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        self.patching = false;
        let volumes = if self.granted {
            self.volumes.clone()
        } else {
            Vec::new()
        };
        (volumes, std::mem::take(&mut self.changes))
    }

    fn patch(&mut self, mark: u64) {
        self.patching = true;
        self.patched = Some(self.patched.map_or(mark, |patched| patched.min(mark)));
    }

    fn reread(&mut self, paths: Vec<PathBuf>, mark: u64) {
        if let Some(failed) = self.failed {
            self.want(failed);
        }
        if self.wanted {
            self.want(mark);
            return;
        }
        for path in paths {
            if !self.changes.contains(&path) {
                self.changes.push(path);
            }
        }
        self.patch(mark);
    }

    /// Forgets a volume that could not be read, so the window may add it again. Returns
    /// whether it was still wanted.
    fn forget(&mut self, volume: &Path) -> bool {
        let at = self.volumes.iter().position(|held| held == volume);
        at.map(|at| self.volumes.remove(at)).is_some()
    }
}

/// What one turn of the rebuild thread builds.
struct Pass {
    /// Full Disk Access came on since the last turn.
    full_disk: bool,
    /// Whether the new list keeps clutter, or `None` when only volumes changed, which
    /// leaves the newest list's root as it is.
    clutter: Option<Clutter>,
    /// The mark of the first change the pass reads.
    mark: u64,
    /// The FSEvents event id the list it brings is current to.
    current: u64,
    volumes: Vec<PathBuf>,
    /// Folders the newest list holds to read again; a whole new list reads them all.
    changes: Vec<PathBuf>,
}

/// What the newest list misses of the folders that changed.
#[derive(Debug, PartialEq)]
enum Missed {
    Nothing,
    /// Only these folders, read again in place.
    Folders(Vec<PathBuf>),
    /// Your ignore list may have changed, so only a new list by the new rules is sure.
    More,
}

fn missed<P: AsRef<Path>>(scope: &Scope, list: &RwLock<Listed>, paths: &[P]) -> Missed {
    let mut folders = Vec::new();
    for path in paths.iter().map(AsRef::as_ref) {
        if scope.changes_rules(path) {
            return Missed::More;
        }
        if !holds(scope, list, path) {
            folders.push(path.to_path_buf());
        }
    }
    if folders.is_empty() {
        Missed::Nothing
    } else {
        Missed::Folders(folders)
    }
}

/// Whether `list` already matches what a scan by `scope` would keep of `path`. Looks one folder
/// at a time, so a catch-up of thousands at launch never holds a patch, and searches, back.
fn holds(scope: &Scope, list: &RwLock<Listed>, path: &Path) -> bool {
    read(list).is_some_and(|listed| scope.holds(path, &listed.index))
}

/// A list searches read, patched in place as files change. Each patch moves `version` on, since
/// it may number the entries anew.
struct Listed {
    index: Index,
    version: u64,
}

type List = Arc<RwLock<Listed>>;

fn list_of(index: Index) -> List {
    Arc::new(RwLock::new(Listed { index, version: 0 }))
}

/// `None` once a bug stopped a patch halfway: that list answers nothing until a whole new one
/// replaces it.
fn read(list: &RwLock<Listed>) -> Option<RwLockReadGuard<'_, Listed>> {
    list.read().ok()
}

/// The newest list, shared with the worker rather than copied, and the rules it was built by.
type Newest = Option<(Arc<Scope>, List)>;

/// The folders to watch for changes to the newest list, and the mark of the first change its
/// rebuild was asked for, or 0 for the first list.
type Watched = (Vec<(String, Watch)>, u64);

struct Live {
    /// How far apart rebuilds start while files keep changing.
    gap: Duration,
    plan: Mutex<Plan>,
    wake: Condvar,
    /// None for made-up names, or before the first list.
    newest: Mutex<Newest>,
    watched: Mutex<Watched>,
    opens: Mutex<Opens>,
    /// Taken around each read or save of the opens' file, so it never goes back to an older
    /// list, while searches never wait for the disk.
    saves: Mutex<()>,
    /// Counts volumes taken out, so a scan of one stops at once and never holds up its eject.
    ejected: AtomicU64,
    /// Held while volumes are read, so a drive taken out is approved for unmounting only once
    /// its stopped scan has closed every folder on it.
    reading: Mutex<()>,
    save_after: Duration,
    save_within: Duration,
    resave: Duration,
    saving: Mutex<Saving>,
    save: Condvar,
    /// Held around each save of the list and each patch of it, so a patch waits out a long save
    /// without searches queuing behind it.
    writing: Mutex<()>,
    /// Its `busy` covers only the first list; the plan tells of later ones.
    health: Mutex<TlHealth>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            gap: GAP,
            plan: Mutex::default(),
            wake: Condvar::new(),
            newest: Mutex::default(),
            watched: Mutex::default(),
            opens: Mutex::default(),
            saves: Mutex::default(),
            ejected: AtomicU64::default(),
            reading: Mutex::default(),
            save_after: SAVE_AFTER,
            save_within: SAVE_WITHIN,
            resave: RESAVE,
            saving: Mutex::default(),
            save: Condvar::new(),
            writing: Mutex::default(),
            health: Mutex::default(),
        }
    }
}

/// The newest list not saved yet, for a core given a file to save to.
#[derive(Default)]
struct Saving {
    file: Option<PathBuf>,
    waiting: Option<Unsaved>,
    kept: Option<Kept>,
    stopped: bool,
}

struct Kept {
    settings: Arc<Settings>,
    unreadable: usize,
    event_id: u64,
    /// When it was handed to the saver, or `None` for the list opened at launch.
    at: Option<Instant>,
}

struct Unsaved {
    list: List,
    settings: Arc<Settings>,
    unreadable: usize,
    event_id: u64,
    /// When the first list not saved yet came, and the newest.
    first: Instant,
    last: Instant,
}

impl Live {
    fn plan(&self, change: impl FnOnce(&mut Plan)) {
        change(&mut self.plan.lock().unwrap());
        self.wake.notify_all();
    }

    /// Keeps the list and its rules, for judging later changes, and hands the list on.
    fn publish(&self, built: Built, mark: u64) -> List {
        self.publish_list(list_of(built.index), built.scope, mark)
    }

    /// As `publish`, for a list already shared, as one just patched.
    fn publish_list(&self, list: List, scope: Option<Scope>, mark: u64) -> List {
        let (watched, volumes): (_, Vec<PathBuf>) = match (&scope, read(&list)) {
            (Some(scope), Some(listed)) => (
                scope.watched(&listed.index),
                listed.index.volumes().map(Path::to_path_buf).collect(),
            ),
            _ => Default::default(),
        };
        // A list read before a drive was ejected still holds it; its watch must not come back.
        let plan = self.plan.lock().unwrap();
        let watched = watched
            .into_iter()
            .filter(|(path, _)| plan.volumes.contains(path) || !volumes.contains(path))
            .map(|(path, watch)| (path.to_string_lossy().into_owned(), watch))
            .collect();
        *self.watched.lock().unwrap() = (watched, mark);
        drop(plan);
        *self.newest.lock().unwrap() = scope.map(|scope| (Arc::new(scope), Arc::clone(&list)));
        list
    }

    /// Notes a new list coming into use for `tl_health`: read from the whole folder when it
    /// `took` that long, and after the first one when `later`.
    fn note_list(&self, entries: usize, took: Option<Duration>, later: bool) {
        let now = unix_now();
        let mut health = lock(&self.health);
        health.entries = entries as u64;
        health.busy = false;
        if let Some(took) = took {
            health.scanned_at = now;
            health.scan_ms = took.as_millis().try_into().unwrap_or(u64::MAX);
        }
        if later {
            health.changed_at = now;
        }
    }

    fn keep(&self, list: &List, source: &Source, event_id: u64) {
        let Some((settings, unreadable)) = source.saving() else {
            return;
        };
        let mut saving = self.saving.lock().unwrap();
        if saving.file.is_none() {
            return;
        }
        let now = Instant::now();
        let first = saving.waiting.as_ref().map_or(now, |waiting| waiting.first);
        saving.kept = Some(Kept {
            settings: Arc::clone(&settings),
            unreadable,
            event_id,
            at: Some(now),
        });
        saving.waiting = Some(Unsaved {
            list: Arc::clone(list),
            settings,
            unreadable,
            event_id,
            first,
            last: now,
        });
        drop(saving);
        self.save.notify_all();
    }

    fn opened(&self, source: &Source, event_id: u64) {
        let Some((settings, unreadable)) = source.saving() else {
            return;
        };
        self.saving.lock().unwrap().kept = Some(Kept {
            settings,
            unreadable,
            event_id,
            at: None,
        });
    }

    /// For a change the newest list needed nothing for, while nothing else waited: without
    /// this, a week of edits that change no names would all be replayed at the next launch.
    fn caught_up(&self, event_id: u64) {
        let mut guard = self.saving.lock().unwrap();
        let saving = &mut *guard;
        if let Some(waiting) = &mut saving.waiting {
            waiting.event_id = waiting.event_id.max(event_id);
            return;
        }
        let due = |kept: &&mut Kept| {
            kept.event_id < event_id && kept.at.is_none_or(|at| at.elapsed() >= self.resave)
        };
        let Some(kept) = saving.kept.as_mut().filter(due) else {
            return;
        };
        let Some((_, list)) = self.newest.lock().unwrap().clone() else {
            return;
        };
        let now = Instant::now();
        (kept.event_id, kept.at) = (event_id, Some(now));
        saving.waiting = Some(Unsaved {
            list,
            settings: Arc::clone(&kept.settings),
            unreadable: kept.unreadable,
            event_id,
            first: now,
            last: now,
        });
        drop(guard);
        self.save.notify_all();
    }

    fn misses(&self, paths: &[PathBuf]) -> Missed {
        let newest = self.newest.lock().unwrap().clone();
        newest.map_or(Missed::Nothing, |(scope, list)| {
            missed(&scope, &list, paths)
        })
    }

    /// Adds each of `volumes` to `index`. One taken out meanwhile stops its scan and is left
    /// out; one that cannot be read is forgotten, so the window may add it again, and told
    /// as a note. Returns whether any joined.
    fn add_volumes(
        &self,
        settings: &Settings,
        index: &mut Index,
        volumes: &[PathBuf],
        notify: &Notify,
    ) -> bool {
        let mut joined = false;
        for volume in volumes {
            let _reading = self.reading.lock().unwrap();
            // Ejected while the home folder was read, before this pass reached the drive.
            let ejected = {
                let plan = self.plan.lock().unwrap();
                if !plan.volumes.contains(volume) {
                    continue;
                }
                self.ejected.load(Ordering::Relaxed)
            };
            let stop = || self.ejected.load(Ordering::Relaxed) != ejected;
            match settings.add_volume_until(index, volume, &stop) {
                Ok(_) => joined = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    if self.plan.lock().unwrap().forget(volume) {
                        notify.note(&format!("{} could not be read: {error}", volume.display()));
                    }
                }
            }
        }
        joined
    }
}

/// What a core lists, kept so its rebuild thread can build it again.
enum Source {
    StandIn(usize),
    Home {
        root: PathBuf,
        access: Access,
        /// Problems with the ignore list already told, so a rebuild doesn't repeat them.
        told: Vec<String>,
        /// What the newest list was built with, so a volume joins it by the same rules.
        settings: Option<Arc<Settings>>,
        /// The FSEvents log the saved list belongs to, for a core that saves one.
        journal: Option<Vec<u8>>,
        /// Folders the newest list's scan could not read in full.
        unreadable: usize,
    },
}

struct Built {
    index: Index,
    scope: Option<Scope>,
}

impl Source {
    /// Scans the root, then adds each of `volumes`, as `Live::add_volumes` does.
    fn build(
        &mut self,
        clutter: Clutter,
        volumes: &[PathBuf],
        live: &Live,
        notify: &Notify,
    ) -> io::Result<Built> {
        if let Self::StandIn(count) = self {
            let index = argus_core::stand_in(*count);
            return Ok(Built { index, scope: None });
        }
        let settings = self.settings(clutter, notify)?;
        let scan = settings.scan()?;
        let mut index = scan.index;
        live.add_volumes(&settings, &mut index, volumes, notify);
        let scope = Some(settings.scope(&index));
        self.built_with(settings, scan.unreadable_folders);
        Ok(Built { index, scope })
    }

    /// The list saved in `file` and the FSEvents event id it is current to, when it was saved
    /// with the settings a scan would use now. Its volumes are taken out: the window adds
    /// back those still mounted.
    fn open(&mut self, file: &Path, clutter: Clutter, notify: &Notify) -> Option<(Built, u64)> {
        let settings = self.settings(clutter, notify).ok()?;
        let saved = match open_index(file, &settings) {
            Ok(saved) => saved,
            Err(why) => {
                let expected = match &why {
                    OpenError::Io(error) => error.kind() == io::ErrorKind::NotFound,
                    // Full Disk Access, the ignore list or Argus itself changed.
                    OpenError::OtherSettings | OpenError::Version(_) => true,
                    _ => false,
                };
                if !expected {
                    notify.note(&format!("the saved list could not be opened: {why}"));
                }
                return None;
            }
        };
        let mut index = saved.scan.index;
        let volumes: Vec<PathBuf> = index.volumes().map(Path::to_path_buf).collect();
        for volume in &volumes {
            settings.remove_volume(&mut index, volume);
        }
        let scope = Some(settings.scope(&index));
        self.built_with(settings, saved.scan.unreadable_folders);
        Some((Built { index, scope }, saved.event_id))
    }

    fn built_with(&mut self, settings: Settings, folders: usize) {
        if let Self::Home {
            settings: last,
            unreadable,
            ..
        } = self
        {
            *last = Some(Arc::new(settings));
            *unreadable = folders;
        }
    }

    fn saving(&self) -> Option<(Arc<Settings>, usize)> {
        match self {
            Self::Home {
                settings: Some(settings),
                unreadable,
                ..
            } => Some((Arc::clone(settings), *unreadable)),
            _ => None,
        }
    }

    /// What a home folder's list is built with now, reading the ignore list again.
    fn settings(&mut self, clutter: Clutter, notify: &Notify) -> io::Result<Settings> {
        let Self::Home {
            root,
            access,
            told,
            journal,
            ..
        } = self
        else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        let access = *access;
        // The folder being scanned, not $HOME, which a launch from a terminal may change.
        let home = std::fs::canonicalize(root)?;
        // Read again each time, so an edited list counts from the next rebuild on. A list that
        // keeps everything leaves it out, so its problems don't matter then.
        let ignore = match clutter {
            Clutter::Keep => Ignore::default(),
            Clutter::Skip => {
                let (ignore, problems) = Ignore::load(&home, access);
                if problems != *told {
                    for problem in &problems {
                        notify.note(problem);
                    }
                    *told = problems;
                }
                ignore
            }
        };
        let settings = Settings::new(&home, &home, clutter, ignore, access);
        Ok(match journal {
            Some(journal) => settings.with_journal(journal),
            None => settings,
        })
    }

    /// The newest list with the volumes added or taken out to match `volumes` and the
    /// folders in `changes` read again, its root left as it is, and its rules, or `None` when
    /// that changes nothing. Without reading the home folder, the list in use is patched while
    /// new searches wait. A drive joining takes seconds to read, so it joins a copy instead,
    /// with the memory of a second list for that moment.
    fn graft(
        &self,
        volumes: &[PathBuf],
        changes: &[PathBuf],
        live: &Live,
        notify: &Notify,
    ) -> Option<(List, Option<Scope>)> {
        let Self::Home {
            settings: Some(settings),
            ..
        } = self
        else {
            return None;
        };
        let list = live.newest.lock().unwrap().clone()?.1;
        let (gone, new) = {
            let listed = read(&list)?;
            let gone: Vec<PathBuf> = listed
                .index
                .volumes()
                .filter(|&held| !volumes.iter().any(|volume| volume == held))
                .map(Path::to_path_buf)
                .collect();
            let new: Vec<PathBuf> = volumes
                .iter()
                .filter(|volume| !listed.index.volumes().any(|held| held == volume.as_path()))
                .cloned()
                .collect();
            (gone, new)
        };
        if gone.is_empty() && new.is_empty() && changes.is_empty() {
            return None;
        }
        if new.is_empty() {
            let mut listed = list.write().ok()?;
            for volume in &gone {
                settings.remove_volume(&mut listed.index, volume);
            }
            settings.update(&mut listed.index, changes);
            listed.version += 1;
            let scope = settings.scope(&listed.index);
            drop(listed);
            return Some((list, Some(scope)));
        }
        let mut index = Index::clone(&read(&list)?.index);
        drop(list);
        for volume in &gone {
            settings.remove_volume(&mut index, volume);
        }
        // Before the new volumes join, which are read whole.
        settings.update(&mut index, changes);
        let joined = live.add_volumes(settings, &mut index, &new, notify);
        // A change read again is promised a new list, even one that lists the same.
        let changed = joined || !gone.is_empty() || !changes.is_empty();
        changed.then(|| {
            let scope = settings.scope(&index);
            (list_of(index), Some(scope))
        })
    }
}

#[derive(Clone, Copy)]
struct Notify {
    ctx: *mut c_void,
    on_event: TlEventFn,
}

// SAFETY: the window promises `ctx` stays usable from any thread until `tl_core_free` returns.
unsafe impl Send for Notify {}
// SAFETY: as for `Send`; `TlEventFn` tells the window calls may come from several threads at
// once, as text search threads each report the lines they find.
unsafe impl Sync for Notify {}

impl Notify {
    fn send(&self, event: u32, value: u64) {
        // SAFETY: as promised to `tl_core_new`.
        unsafe { (self.on_event)(self.ctx, event, value) }
    }

    fn note(&self, text: &str) {
        let text = CString::new(text).unwrap_or_default();
        self.send(TL_EVENT_NOTE, text.as_ptr() as u64);
    }
}

/// A Rust panic that reached Swift would close the app, so it becomes an error code instead.
fn guard<T>(failed: T, body: impl FnOnce() -> T) -> T {
    panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or(failed)
}

#[unsafe(no_mangle)]
pub extern "C" fn tl_abi_version() -> u32 {
    TL_ABI_VERSION
}

/// Starts a core that scans `root` in the background as a home folder (the app passes yours),
/// skipping clutter and the ignore list inside it, or listing everything when `everything`
/// isn't 0, as `tl_index_everything` does. Without `full_disk` (0), the folders macOS guards
/// with a pop-up each are left unread. Searches sent before `TL_EVENT_INDEXED` wait for it; a
/// scan that fails sends `TL_EVENT_FAILED`, and each problem with the ignore list a
/// `TL_EVENT_NOTE` first. Returns null on failure, without `on_event`, or for a `root`, or a
/// file or journal in `saved`, that isn't UTF-8.
///
/// With `saved`, the list saved in its file opens in place of the first scan when it was
/// saved with the same settings and journal, and `TL_EVENT_OPENED` comes instead of
/// `TL_EVENT_INDEXED`. When the FSEvents event id it was saved at is 0 or newer than `now`,
/// a new list is built in the background and `TL_EVENT_REBUILT` follows; otherwise
/// `tl_watched` hands that id back, so the window can replay what changed since. Each new list
/// is saved to the file in the background, once files have stopped changing for 2 s, making
/// its folder when missing; a save that fails is told as a `TL_EVENT_NOTE`.
///
/// # Safety
/// `root` points to `root_len` bytes, and `saved` is null or points to a `TlSaved` whose
/// pointers each point to their length in bytes. `on_event` may be called with `ctx` from any
/// thread until `tl_core_free` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_core_new(
    root: *const u8,
    root_len: usize,
    full_disk: u32,
    everything: u32,
    saved: *const TlSaved,
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
) -> *mut TlCore {
    guard(std::ptr::null_mut(), || {
        let Some(root) = (unsafe { path_from(root, root_len) }) else {
            return std::ptr::null_mut();
        };
        let access = match full_disk {
            0 => Access::Limited,
            _ => Access::Full,
        };
        let mut journal = None;
        let mut resume = None;
        if let Some(saved) = unsafe { saved.as_ref() } {
            let file = unsafe { path_from(saved.file, saved.file_len) };
            let named = match saved.journal_len {
                0 => Some(&[][..]),
                _ if saved.journal.is_null() => None,
                len => Some(unsafe { std::slice::from_raw_parts(saved.journal, len) }),
            };
            let file = file.filter(|file| !file.as_os_str().is_empty());
            let (Some(file), Some(Ok(named))) = (file, named.map(std::str::from_utf8)) else {
                return std::ptr::null_mut();
            };
            journal = Some(named.as_bytes().to_vec());
            resume = Some((file.to_path_buf(), saved.now));
        }
        let source = Source::Home {
            root: root.to_path_buf(),
            access,
            told: Vec::new(),
            settings: None,
            journal,
            unreadable: 0,
        };
        start(
            ctx,
            on_event,
            source,
            everything != 0,
            resume,
            Live::default(),
        )
    })
}

/// Starts a core over `count` made-up names, built in the background. Searches sent before
/// `TL_EVENT_INDEXED` wait for it. Text searches read real folders, leaving those macOS guards
/// unread until `tl_grant_full_disk`. Returns null on failure, without `on_event`, or for a
/// `count` too big to number, since entry numbers are 32-bit.
///
/// # Safety
/// `on_event` may be called with `ctx` from any thread until `tl_core_free` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_core_new_stand_in(
    count: u64,
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
) -> *mut TlCore {
    guard(std::ptr::null_mut(), || {
        if count >= u64::from(u32::MAX) {
            return std::ptr::null_mut();
        }
        let source = Source::StandIn(count as usize);
        start(ctx, on_event, source, false, None, Live::default())
    })
}

/// The saved list's file, and the newest FSEvents event id as the core starts.
type Resume = (PathBuf, u64);

/// Starts the core's threads over `live`, whose plan and saving it fills in.
fn start(
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
    source: Source,
    everything: bool,
    resume: Option<Resume>,
    live: Live,
) -> *mut TlCore {
    let Some(on_event) = on_event else {
        return std::ptr::null_mut();
    };
    let searcher = Arc::new(Searcher::default());
    let done = Arc::default();
    let (jobs, inbox) = mpsc::channel();
    let notify = Notify { ctx, on_event };
    let (home, full_disk, judge) = match &source {
        // Real folders all the same, so no pop-ups before `tl_grant_full_disk`.
        Source::StandIn(_) => (argus_core::home(), false, None),
        Source::Home { root, access, .. } => {
            let judge: Judge = argus_core::check_volume;
            (root.clone(), *access == Access::Full, Some(judge))
        }
    };
    let mut plan = Plan::new(everything, full_disk);
    plan.delivered = resume.as_ref().map_or(0, |&(_, now)| now);
    let live = Arc::new(Live {
        plan: Mutex::new(plan),
        saving: Mutex::new(Saving {
            file: resume.as_ref().map(|(file, _)| file.clone()),
            ..Saving::default()
        }),
        // Until the first list is in use.
        health: Mutex::new(TlHealth {
            busy: true,
            ..TlHealth::default()
        }),
        ..live
    });
    let text = Text {
        home,
        full_disk: AtomicBool::new(full_disk),
        notify,
        started: OnceLock::new(),
    };
    let saved_in = resume
        .as_ref()
        .and_then(|(file, _)| file.parent())
        .map(|folder| std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf()));
    let saver = resume.is_some().then(|| {
        let live = Arc::clone(&live);
        thread::Builder::new()
            .name("saver".into())
            .spawn(move || save_loop(&live, notify))
            .expect("start the saver thread")
    });
    let worker = {
        let (searcher, done, live) = (Arc::clone(&searcher), Arc::clone(&done), Arc::clone(&live));
        let swaps = jobs.clone();
        thread::Builder::new()
            .name("hatch".into())
            .spawn(move || work(source, &searcher, &inbox, swaps, &done, &live, notify))
            .expect("start the hatch worker")
    };
    Box::into_raw(Box::new(TlCore {
        searcher,
        jobs: Some(jobs),
        done,
        live,
        worker: Some(worker),
        saver,
        saved_in,
        text: Some(text),
        judge,
    }))
}

fn first_list(source: &mut Source, live: &Live, notify: &Notify) -> io::Result<(List, bool)> {
    let (clutter, now) = {
        let mut plan = live.plan.lock().unwrap();
        (plan.list(), plan.delivered)
    };
    let file = live.saving.lock().unwrap().file.clone();
    if let Some((built, saved_at)) = file.and_then(|file| source.open(&file, clutter, notify)) {
        // An id from a log since reset, or from a list saved without one, can't be caught up
        // from, so a new list follows, as after a change macOS lost track of.
        let caught_up = saved_at != 0 && saved_at <= now;
        let list = live.publish(built, if caught_up { saved_at } else { 0 });
        if caught_up {
            live.opened(source, saved_at);
        }
        live.plan(|plan| {
            if caught_up {
                plan.delivered = saved_at;
            } else {
                plan.want(0);
            }
        });
        return Ok((list, true));
    }
    let volumes = {
        let mut plan = live.plan.lock().unwrap();
        plan.patched = None;
        plan.volumes().0
    };
    let built = source.build(clutter, &volumes, live, notify)?;
    let list = live.publish(built, 0);
    live.keep(&list, source, now);
    Ok((list, false))
}

/// Builds or opens the first list, then answers searches. The rebuild thread it starts holds
/// `swaps`, so the queue closes only once both the core and that thread are gone.
fn work(
    mut source: Source,
    searcher: &Searcher,
    inbox: &Receiver<Job>,
    swaps: Sender<Job>,
    done: &Done,
    live: &Arc<Live>,
    notify: Notify,
) {
    let started = Instant::now();
    let first = panic::catch_unwind(AssertUnwindSafe(|| first_list(&mut source, live, &notify)));
    let (list, opened) = match first {
        Ok(Ok(first)) => first,
        failed => {
            live.plan(|plan| plan.stopped = true);
            let code = match failed {
                Ok(Err(error)) => error.raw_os_error().map_or(0, |code| code as u64),
                _ => 0,
            };
            notify.send(TL_EVENT_FAILED, code);
            return;
        }
    };
    let event = if opened {
        TL_EVENT_OPENED
    } else {
        TL_EVENT_INDEXED
    };
    let entries = read(&list).map_or(0, |listed| listed.index.entry_count());
    live.note_list(entries, (!opened).then(|| started.elapsed()), false);
    notify.send(event, entries as u64);
    let rebuilds = {
        let live = Arc::clone(live);
        thread::Builder::new()
            .name("rebuild".into())
            .spawn(move || {
                rebuild_loop(&live, &Wall, started, |pass| {
                    rebuild(&mut source, pass, &live, &swaps, &notify);
                });
            })
            .expect("start the rebuild thread")
    };
    let served = panic::catch_unwind(AssertUnwindSafe(|| {
        serve(list, searcher, inbox, done, &live.opens, &notify, RERANK);
    }));
    if served.is_err() {
        live.plan(|plan| plan.stopped = true);
        notify.send(TL_EVENT_FAILED, 0);
    }
    // Waited for even after a bug, so it can't call `on_event` once the core is freed.
    let _ = rebuilds.join();
}

fn serve(
    mut list: List,
    searcher: &Searcher,
    inbox: &Receiver<Job>,
    done: &Done,
    opens: &Mutex<Opens>,
    notify: &Notify,
    rerank: Duration,
) {
    let mut apps = Apps::default();
    // Each opened file's entry is found once per list or run of opens, never per search, on a
    // copy so `tl_opened` never waits for it.
    // Timed by the wall clock: `Instant` stands still while the Mac sleeps, and scores fade
    // through a three-week sleep all the same.
    let rank = |index: &Index, apps: &Apps| {
        let frecency = lock(opens).frecency.clone();
        let now = unix_now();
        (searcher.boosts(&frecency, index, apps, now), now)
    };
    let (mut boosts, mut ranked): (Boosts, u64) = (Boosts::default(), 0);
    let mut stale = true;
    // The folder searched inside, worked out once for every keystroke typed there; `None`
    // beside it when the list holds no such folder.
    let mut inside: Option<(String, Option<Within>)> = None;
    // The version of the list the boosts and `inside` were worked out on: both name entries by
    // number, which a patch may change.
    let mut seen = None;
    loop {
        let job = match inbox.try_recv() {
            Ok(job) => job,
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {
                if stale && let Some(listed) = read(&list) {
                    if seen != Some(listed.version) {
                        (seen, inside) = (Some(listed.version), None);
                    }
                    (boosts, ranked) = rank(&listed.index, &apps);
                    stale = false;
                }
                let Ok(job) = inbox.recv() else { return };
                job
            }
        };
        let (ticket, query, folder) = match job {
            // Searches sent before this one ran on the old list; the old list is freed here.
            Job::Swap { list: fresh, took } => {
                list = fresh;
                let entries = read(&list).map_or(0, |listed| listed.index.entry_count());
                notify.send(TL_EVENT_REBUILT, u64::from(took) << 32 | entries as u64);
                (seen, stale) = (None, true);
                continue;
            }
            Job::Opened => {
                stale = true;
                notify.send(TL_EVENT_RERANKED, 0);
                continue;
            }
            // Read on each show, so the same apps found again cost no new boosts.
            Job::Apps(folders) => {
                let fresh = Apps::scan_folders(folders);
                if fresh != apps {
                    apps = fresh;
                    stale = true;
                    notify.send(TL_EVENT_RERANKED, 0);
                }
                continue;
            }
            Job::Note(note) => {
                notify.note(&note);
                continue;
            }
            Job::Search {
                ticket,
                query,
                folder,
            } => (ticket, query, folder),
        };
        // Waits here while the list is patched.
        let Some(listed) = read(&list) else {
            *done.lock().unwrap() = Some((ticket, Packed::default()));
            notify.send(TL_EVENT_READY, ticket.number());
            continue;
        };
        if seen != Some(listed.version) {
            (seen, inside, stale) = (Some(listed.version), None, true);
        }
        let index = &listed.index;
        if stale || unix_now().abs_diff(ranked) >= rerank.as_secs() {
            (boosts, ranked) = rank(index, &apps);
            stale = false;
        }
        let search =
            |within| searcher.search_with(index, &apps, within, &query, ticket, ROWS, &boosts);
        let hits = match folder {
            None => {
                inside = None;
                search(None)
            }
            Some(folder) => {
                if inside.as_ref().is_none_or(|(path, _)| *path != folder) {
                    let within = Within::folder(index, Path::new(&folder));
                    inside = Some((folder, within));
                }
                match inside.as_ref().and_then(|(_, within)| within.as_ref()) {
                    Some(within) => search(Some(within)),
                    // Gone since you went into it: nothing is inside.
                    None => Some(Vec::new()),
                }
            }
        };
        // A search replaced by a newer keystroke gives up and is never shown.
        let Some(hits) = hits else {
            continue;
        };
        // A key pressed just as the search finished: skip rows the window would refuse.
        if searcher.newest() != ticket.number() {
            continue;
        }
        let packed = pack(index, &apps, &hits);
        drop(listed);
        *done.lock().unwrap() = Some((ticket, packed));
        notify.send(TL_EVENT_READY, ticket.number());
    }
}

/// Time as the rebuild thread sees it, so a test can move it by hand.
trait Clock {
    fn now(&self) -> Instant;
    /// Lets `plan` go until `live.wake` is notified, or `wait` has passed when given.
    fn sleep<'a>(
        &self,
        live: &'a Live,
        plan: MutexGuard<'a, Plan>,
        wait: Option<Duration>,
    ) -> MutexGuard<'a, Plan>;
}

struct Wall;

impl Clock for Wall {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep<'a>(
        &self,
        live: &'a Live,
        plan: MutexGuard<'a, Plan>,
        wait: Option<Duration>,
    ) -> MutexGuard<'a, Plan> {
        match wait {
            None => live.wake.wait(plan).unwrap(),
            Some(wait) => live.wake.wait_timeout(plan, wait).unwrap().0,
        }
    }
}

/// Calls `rebuild` for each rebuild wanted until the core stops: one at a time, with changes
/// made meanwhile waiting as one more, each starting at least `live.gap` after the one before.
/// `last` is when the first list was started. A turn with only volumes to add or take out,
/// or folders to read again, patches the newest list, as `Source::graft` tells.
fn rebuild_loop(live: &Live, clock: &impl Clock, mut last: Instant, mut rebuild: impl FnMut(Pass)) {
    let mut plan = live.plan.lock().unwrap();
    loop {
        if plan.stopped {
            return;
        }
        let whole = plan.wanted || plan.everything != plan.everything_listed;
        if !whole && !plan.patching {
            plan = clock.sleep(live, plan, None);
            continue;
        }
        let now = clock.now();
        if now < last + live.gap {
            plan = clock.sleep(live, plan, Some(last + live.gap - now));
            continue;
        }
        plan.wanted = false;
        plan.running = true;
        plan.whole = whole;
        plan.listed = false;
        let full_disk = std::mem::take(&mut plan.full_disk);
        let mark = [plan.asked.take(), plan.switched, plan.patched.take()];
        let mark = mark.into_iter().flatten().min().unwrap_or_default();
        let clutter = whole.then(|| plan.list());
        let (volumes, changes) = plan.volumes();
        let current = plan.delivered;
        drop(plan);
        last = now;
        rebuild(Pass {
            full_disk,
            clutter,
            mark,
            current,
            volumes,
            changes: if whole { Vec::new() } else { changes },
        });
        plan = live.plan.lock().unwrap();
        plan.running = false;
        if let Some((paths, mark)) = plan.recheck.take() {
            drop(plan);
            let missed = live.misses(&paths);
            plan = live.plan.lock().unwrap();
            match missed {
                Missed::Nothing => {}
                Missed::Folders(paths) => plan.reread(paths, mark),
                Missed::More => plan.want(mark),
            }
        }
    }
}

/// A failed rebuild keeps the list in use and is told as a note.
fn rebuild(source: &mut Source, pass: Pass, live: &Live, swaps: &Sender<Job>, notify: &Notify) {
    if let (true, Source::Home { access, .. }) = (pass.full_disk, &mut *source) {
        *access = Access::Full;
    }
    let started = Instant::now();
    let whole = pass.clutter.is_some();
    // Held until the patched list is kept, so a save never pairs it with an older event.
    let _writing = (!whole).then(|| lock(&live.writing));
    let built = panic::catch_unwind(AssertUnwindSafe(|| match pass.clutter {
        Some(clutter) => source
            .build(clutter, &pass.volumes, live, notify)
            .map(|built| Some(live.publish(built, pass.mark))),
        None => Ok(source
            .graft(&pass.volumes, &pass.changes, live, notify)
            .map(|(list, scope)| live.publish_list(list, scope, pass.mark))),
    }))
    .unwrap_or_else(|_| Err(io::Error::other("a bug in the engine")));
    match built {
        Ok(Some(list)) => {
            let elapsed = started.elapsed();
            let entries = read(&list).map_or(0, |listed| listed.index.entry_count());
            live.note_list(entries, whole.then_some(elapsed), true);
            let took = elapsed.as_millis().min(u32::MAX.into()) as u32;
            let missing = {
                let mut plan = live.plan.lock().unwrap();
                if whole {
                    plan.failed = None;
                }
                plan.listed = true;
                plan.failed.is_some()
            };
            if !missing {
                live.keep(&list, source, pass.current);
            }
            // Fails only once the worker has stopped.
            let _ = swaps.send(Job::Swap { list, took });
        }
        Ok(None) => {}
        Err(error) => {
            let newest = live.newest.lock().unwrap().clone();
            let broken = newest.is_some_and(|(_, list)| list.is_poisoned());
            live.plan(|plan| {
                plan.failed = Some(
                    plan.failed
                        .map_or(pass.mark, |failed| failed.min(pass.mark)),
                );
                // A patch stopped halfway, so search finds nothing until a whole list is read.
                if broken {
                    plan.want(pass.mark);
                }
            });
            notify.note(&format!("the list could not be built again: {error}"));
        }
    }
}

/// A save that fails is told as a note, and the next list tries again.
fn save_loop(live: &Live, notify: Notify) {
    let file = live.saving.lock().unwrap().file.clone();
    if let Some(file) = file {
        remove_old_temps(&file, LEFT_FOR);
    }
    let mut saving = live.saving.lock().unwrap();
    loop {
        if saving.stopped {
            return;
        }
        let Some(waiting) = &saving.waiting else {
            saving = live.save.wait(saving).unwrap();
            continue;
        };
        let due = (waiting.last + live.save_after).min(waiting.first + live.save_within);
        let now = Instant::now();
        if now < due {
            saving = live.save.wait_timeout(saving, due - now).unwrap().0;
            continue;
        }
        drop(saving);
        // Taken only now, past any patch under way, so the list saved is the one it kept.
        let writing = lock(&live.writing);
        let (file, list) = {
            let mut saving = live.saving.lock().unwrap();
            (saving.file.clone(), saving.waiting.take())
        };
        if let (Some(file), Some(list)) = (file, list) {
            let saved = panic::catch_unwind(AssertUnwindSafe(|| save(&file, &list)))
                .unwrap_or_else(|_| Err(io::Error::other("a bug in the engine")));
            drop(list);
            if let Err(error) = saved {
                notify.note(&format!("the list could not be saved: {error}"));
            }
        }
        drop(writing);
        saving = live.saving.lock().unwrap();
    }
}

fn save(file: &Path, list: &Unsaved) -> io::Result<()> {
    // Broken by a bug while patched; the whole list read next is saved instead.
    let Some(listed) = read(&list.list) else {
        return Ok(());
    };
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    save_index(
        file,
        &listed.index,
        list.unreadable,
        &list.settings,
        list.event_id,
    )
}

fn pack(index: &Index, apps: &Apps, hits: &[Hit]) -> Packed {
    let mut packed = Packed {
        rows: Vec::with_capacity(hits.len()),
        text: String::new(),
    };
    for hit in hits {
        let (name, path, is_folder) = match hit.item {
            Item::Entry(entry) => (index.name(entry), index.path(entry), index.is_dir(entry)),
            Item::App(app) => (apps.name(app), apps.path(app), false),
        };
        let folder = path.parent().unwrap_or(&path).to_string_lossy();
        let name_start = packed.text.len() as u32;
        packed.text.push_str(name);
        let folder_start = packed.text.len() as u32;
        packed.text.push_str(&folder);
        packed.rows.push(TlRow {
            name_start,
            name_len: folder_start - name_start,
            folder_start,
            folder_len: folder.len() as u32,
            is_folder,
            is_app: matches!(hit.item, Item::App(_)),
        });
    }
    packed
}

/// Starts a search and returns its ticket, or 0 on failure. Every older search gives up. With a
/// `folder`, a full path as the rows give it, only what is inside that folder is found, ranked
/// as in a search of everything; a folder the list doesn't hold finds nothing. Without one
/// (`folder_len` 0), everything is searched.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, `query` points to `len` bytes of
/// UTF-8 (or `len` is 0), and `folder` to `folder_len` (or `folder_len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_submit(
    core: *const TlCore,
    query: *const u8,
    len: usize,
    folder: *const u8,
    folder_len: usize,
) -> u64 {
    guard(0, || {
        let core = unsafe { &*core };
        // An empty search box can arrive as a null pointer, which a slice must never hold.
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(query, len) },
        };
        let folder = match folder_len {
            0 => None,
            _ => {
                let bytes = unsafe { std::slice::from_raw_parts(folder, folder_len) };
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
        };
        let ticket = core.searcher.ticket();
        let query = String::from_utf8_lossy(bytes).into_owned();
        let job = Job::Search {
            ticket,
            query,
            folder,
        };
        // Sending fails only once the worker has stopped, which it reported as TL_EVENT_FAILED.
        let sent = core
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok());
        if sent { ticket.number() } else { 0 }
    })
}

/// Reads the apps in `folders`, and in the plain folders directly inside them, in the
/// background before the next search, which then finds them by name beside the files, as rows
/// with `is_app`. Replaces the apps read before, so calling this again picks up apps installed
/// or deleted since; `TL_EVENT_RERANKED` follows when they differ. `folders` holds `len` bytes:
/// full paths in UTF-8, each ending in a NUL byte; none (`len` 0) leaves searches without apps.
/// A folder that is missing, unreadable, relative or not UTF-8 adds nothing. Returns 1 once
/// queued, or 0 once the core has stopped. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `folders` points to `len` bytes
/// (or `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_apps(core: *const TlCore, folders: *const u8, len: usize) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(folders, len) },
        };
        let folders = bytes
            .split(|&byte| byte == 0)
            .filter_map(|folder| std::str::from_utf8(folder).ok())
            .map(PathBuf::from)
            .filter(|folder| folder.is_absolute())
            .collect();
        let sent = core
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job::Apps(folders)).is_ok());
        u32::from(sent)
    })
}

/// Tells the core that folders changed. `paths` holds `len` bytes: real paths in UTF-8, each
/// ending in a NUL byte. Each folder the list reads whose names it no longer matches is read
/// again into the list in the background, as searches wait; the whole list is built again
/// when one may hold your ignore list, or whatever they are when `rescan` isn't 0, as when macOS
/// dropped changes. `TL_EVENT_REBUILT` follows either way. `mark` is the id of the newest
/// FSEvents event among them, handed back by `tl_watched`, or 0 for a change FSEvents didn't
/// report, which counts as the newest id told so far. Returns 1 when a new list is coming,
/// else 0, as once the core has stopped. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `paths` points to `len` bytes
/// (or `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_changed(
    core: *const TlCore,
    paths: *const u8,
    len: usize,
    rescan: u32,
    mark: u64,
) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let live = &core.live;
        // Each change is heard only once it is in the plan, so a list started after it is
        // never saved as current to an event it hasn't read.
        let (running, everything, mark) = {
            let mut plan = live.plan.lock().unwrap();
            if plan.stopped {
                return 0;
            }
            let mark = if mark == 0 { plan.delivered } else { mark };
            if let (Some(failed), false) = (plan.failed, plan.running) {
                plan.want(failed);
                live.wake.notify_all();
            }
            // A rebuild not started yet will read every change made before it starts.
            if plan.wanted {
                plan.want(mark);
                plan.heard(mark);
                return 1;
            }
            (plan.running, plan.everything_listed, mark)
        };
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(paths, len) },
        };
        // With your Library in the list, each save would otherwise bring a new list to save.
        let saver_own = |path: &Path| core.saved_in.as_deref() == Some(path);
        let paths: Option<Vec<&Path>> = bytes
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| std::str::from_utf8(path).ok().map(Path::new))
            .filter(|path| !path.is_some_and(saver_own))
            .collect();
        let newest = live.newest.lock().unwrap().clone();
        let wanted = match (paths, newest) {
            _ if rescan != 0 => true,
            (_, None) => false,
            (None, _) => true,
            (Some(paths), Some((scope, list))) if running => {
                // The rebuild under way may have read these folders before they changed. One
                // keeping everything reads folders the newest list's rules leave out.
                let changed: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|path| {
                        everything || scope.in_scope(path) || !holds(&scope, &list, path)
                    })
                    .map(Path::to_path_buf)
                    .collect();
                let mut plan = live.plan.lock().unwrap();
                if changed.is_empty() {
                    plan.heard(mark);
                    return 0;
                }
                if plan.running {
                    let (waiting, asked) = plan.recheck.get_or_insert_with(|| (Vec::new(), mark));
                    *asked = (*asked).min(mark);
                    for path in changed {
                        if !waiting.contains(&path) {
                            waiting.push(path);
                        }
                    }
                } else if changed.iter().any(|path| scope.changes_rules(path)) {
                    plan.want(mark);
                } else {
                    // It ended just now, maybe without reading them.
                    plan.reread(changed, mark);
                }
                plan.heard(mark);
                drop(plan);
                live.wake.notify_all();
                return 1;
            }
            (Some(paths), Some((scope, list))) => match missed(&scope, &list, &paths) {
                Missed::Nothing => false,
                Missed::More => true,
                // Read again in place, without the rest of the home folder.
                Missed::Folders(paths) => {
                    live.plan(|plan| {
                        plan.reread(paths, mark);
                        plan.heard(mark);
                    });
                    return 1;
                }
            },
        };
        let mut plan = live.plan.lock().unwrap();
        plan.heard(mark);
        if wanted {
            plan.want(mark);
            drop(plan);
            live.wake.notify_all();
            return 1;
        }
        let (idle, delivered) = (plan.idle(), plan.delivered);
        drop(plan);
        if idle {
            live.caught_up(delivered);
        }
        0
    })
}

/// Builds the list again with the folders macOS guards, once Full Disk Access is switched on
/// while the core runs, and lets text searches sent from then on read them too. When
/// `everything` isn't 0, `tl_index_everything` is switched on in the same step, so that one
/// list has both. `mark` is as for `tl_changed`. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grant_full_disk(core: *const TlCore, everything: u32, mark: u64) {
    guard((), || {
        let core = unsafe { &*core };
        if let Some(text) = &core.text {
            text.full_disk.store(true, Ordering::Relaxed);
        }
        core.live.plan(|plan| {
            plan.full_disk = true;
            plan.granted = true;
            plan.everything |= everything != 0;
            plan.want(mark);
        });
    })
}

/// Lists everything in the scanned folder from the next list on when `on` isn't 0, clutter and
/// the folders the ignore list skips included, and skips them again when it is. A whole home
/// folder is about 2.4 million entries, and scanning it takes 10 to 20 s. Returns 1 when that
/// needs a new list, which `TL_EVENT_REBUILT` brings, or `TL_EVENT_INDEXED` when the first
/// list wasn't started yet. Returns 0 when the setting was already so, when the list in use or
/// being built already has it, as when switched back before the rebuild started, or once the
/// core has stopped. `mark` is as for `tl_changed`. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_index_everything(core: *const TlCore, on: u32, mark: u64) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let on = on != 0;
        let mut plan = core.live.plan.lock().unwrap();
        if plan.stopped || plan.everything == on {
            return 0;
        }
        plan.everything = on;
        if plan.everything_listed == on {
            plan.switched = None;
            return 0;
        }
        plan.switched = Some(mark);
        drop(plan);
        core.live.wake.notify_all();
        1
    })
}

/// Tells the core a volume was mounted at `path`, `len` bytes of UTF-8 such as
/// "/Volumes/Photos", so the list reads it too: a drive plugged in, not a file server, a Time
/// Machine backup, an opened disk image or the startup disk under another name. It joins the
/// newest list without the home folder being read again, and `TL_EVENT_REBUILT` follows,
/// unless it is ejected first. One that cannot be read is told as a `TL_EVENT_NOTE` and
/// forgotten, so calling this again tries once more. Without Full Disk Access it is
/// remembered and read once `tl_grant_full_disk` is called, as macOS asks before an app reads
/// an external drive. Returns 1 when that work was queued, else 0, as for a volume already
/// added. `mark` is as for `tl_changed`. Safe to call from any thread, and given the path
/// macOS mounted the volume at, it reads only the system's list of mounts, never the volume.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `path` points to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_volume_added(
    core: *const TlCore,
    path: *const u8,
    len: usize,
    mark: u64,
) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let Some(path) = (unsafe { path_from(path, len) }) else {
            return 0;
        };
        let Some(Ok(volume)) = core.judge.map(|judge| judge(path)) else {
            return 0;
        };
        let mut plan = core.live.plan.lock().unwrap();
        if plan.stopped || plan.volumes.contains(&volume) {
            return 0;
        }
        plan.volumes.push(volume);
        if !plan.granted {
            return 0;
        }
        plan.patch(mark);
        drop(plan);
        core.live.wake.notify_all();
        1
    })
}

/// Tells the core the volume at `path`, by its real path, is going away, so the
/// newest list drops it and everything on it without the home folder being read again;
/// `TL_EVENT_REBUILT` follows. Call it once macOS says the volume will unmount, or has
/// unmounted without warning: `tl_watched` stops naming it at once, even for a list read
/// before, and a scan of it under way stops and has closed every folder on it by the time this
/// returns, while a scan not started yet never starts. Returns 1 when that work was queued, else 0, as
/// for a volume the core was never told of. `mark` is as for `tl_changed`. Safe to call from
/// any thread, and never touches the volume.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `path` points to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_volume_removed(
    core: *const TlCore,
    path: *const u8,
    len: usize,
    mark: u64,
) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let Some(path) = (unsafe { path_from(path, len) }) else {
            return 0;
        };
        let mut plan = core.live.plan.lock().unwrap();
        if !plan.forget(path) {
            return 0;
        }
        core.live.ejected.fetch_add(1, Ordering::Relaxed);
        let mut watched = core.live.watched.lock().unwrap();
        watched.0.retain(|(held, _)| Path::new(held) != path);
        drop(watched);
        if plan.stopped || !plan.granted {
            return 0;
        }
        plan.patch(mark);
        drop(plan);
        core.live.wake.notify_all();
        drop(core.live.reading.lock().unwrap());
        1
    })
}

/// `len` bytes of UTF-8 at `path` as a path, or `None` for a null pointer or other bytes.
///
/// # Safety
/// `path` points to `len` bytes, or is null.
unsafe fn path_from<'a>(path: *const u8, len: usize) -> Option<&'a Path> {
    if path.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path, len) };
    std::str::from_utf8(bytes).ok().map(Path::new)
}

/// The folders to watch for changes to the newest list, sorted, as rows whose names are full
/// paths; none before the first list or for made-up names. Watch everything inside a row with
/// `is_folder`, and only the list of names of one without, such as the scanned folder itself.
/// `since` gets the mark of the first change the list's rebuild was asked for, or 0 for the
/// first list: changes since then may be missing from it. Free the rows with `tl_rows_free`.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, `out` points to a `TlRows` to
/// fill, and `since` to a number.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_watched(core: *const TlCore, out: *mut TlRows, since: *mut u64) -> u32 {
    guard(TL_PANICKED, || {
        let core = unsafe { &*core };
        let watched = core.live.watched.lock().unwrap();
        let (folders, mark) = &*watched;
        let mut packed = Packed {
            rows: Vec::with_capacity(folders.len()),
            text: String::new(),
        };
        for (path, watch) in folders {
            let start = packed.text.len() as u32;
            packed.text.push_str(path);
            let end = packed.text.len() as u32;
            packed.rows.push(TlRow {
                name_start: start,
                name_len: end - start,
                folder_start: end,
                folder_len: 0,
                is_folder: *watch == Watch::Inside,
                is_app: false,
            });
        }
        unsafe { since.write(*mark) };
        drop(watched);
        unsafe { out.write(hand_over(packed)) };
        TL_OK
    })
}

/// Fills `out` with how the list in use is doing, as `TlHealth` explains, and returns `TL_OK`.
/// Never waits for a scan or a search, so the window may call it on each show. Ask for a whole
/// new list with `tl_changed` and a `rescan` that isn't 0. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `out` points to a `TlHealth`
/// to fill.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_health(core: *const TlCore, out: *mut TlHealth) -> u32 {
    guard(TL_PANICKED, || {
        let core = unsafe { &*core };
        let (stopped, busy, building, behind) = {
            let plan = core.live.plan.lock().unwrap();
            (
                plan.stopped,
                plan.busy(),
                plan.building(),
                plan.failed.is_some(),
            )
        };
        let mut health = *lock(&core.live.health);
        // The core's own `busy` covers only the first list, which reads the whole folder.
        health.building = !stopped && (health.busy || building);
        health.busy = !stopped && (health.busy || busy);
        health.behind = behind;
        health.stopped = stopped;
        unsafe { out.write(health) };
        TL_OK
    })
}

/// 1 when the newest list holds the folder at `path`, a full real path of `len` bytes, so a
/// search inside it can find something, else 0, as before the first list or for made-up names.
/// Never waits for a scan, a search or the list being patched: while it is, 1 for a folder the
/// scan reads. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `path` points to `len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_holds_folder(core: *const TlCore, path: *const u8, len: usize) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(path, len) },
        };
        let Ok(path) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let Some((scope, list)) = core.live.newest.lock().unwrap().clone() else {
            return 0;
        };
        let path = Path::new(path);
        // Asked on the window's main thread as the Finder hotkey opens the window.
        let held = match list.try_read() {
            Ok(listed) => listed.index.holds_folder(path),
            Err(TryLockError::WouldBlock) => scope.in_scope(path),
            Err(TryLockError::Poisoned(_)) => false,
        };
        u32::from(held)
    })
}

fn hand_over(packed: Packed) -> TlRows {
    let packed = Box::new(packed);
    TlRows {
        rows: packed.rows.as_ptr(),
        count: packed.rows.len(),
        text: packed.text.as_ptr(),
        owner: Box::into_raw(packed).cast(),
    }
}

/// Hands over the rows for `ticket` once, while no newer search exists. `out` is written
/// only when this returns `TL_OK`.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `out` points to a `TlRows` to fill.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_take(core: *const TlCore, ticket: u64, out: *mut TlRows) -> u32 {
    guard(TL_PANICKED, || {
        let core = unsafe { &*core };
        if ticket != core.searcher.newest() {
            return TL_GONE;
        }
        let mut done = core.done.lock().unwrap();
        let Some((_, packed)) = done.take_if(|(ready, _)| ready.number() == ticket) else {
            return TL_GONE;
        };
        unsafe { out.write(hand_over(packed)) };
        TL_OK
    })
}

/// # Safety
/// `rows` came from `tl_take` and has not been freed yet, or is all zeros.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_rows_free(rows: TlRows) {
    guard((), || {
        if !rows.owner.is_null() {
            drop(unsafe { Box::from_raw(rows.owner.cast::<Packed>()) });
        }
    })
}

/// Ranks files opened often and lately higher, by the opens kept in `file`: read now, in place
/// of any opens counted so far, and written again by each `tl_opened`. A missing file starts an
/// empty list; so does a damaged one, told as a `TL_EVENT_NOTE`, which the next open replaces.
/// One that can't be read or comes from a newer build is told too, and left as it is: opens
/// then count until the core is freed. Returns how many files the list holds, or 0 for an empty
/// `file` or one that isn't UTF-8, which leaves the list as it was. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `file` points to `len` bytes
/// (or `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_frecency_open(core: *const TlCore, file: *const u8, len: usize) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let bytes = match len {
            0 => return 0,
            _ => unsafe { std::slice::from_raw_parts(file, len) },
        };
        let Ok(file) = std::str::from_utf8(bytes).map(PathBuf::from) else {
            return 0;
        };
        let _turn = lock(&core.live.saves);
        let (frecency, file) = match Frecency::open(&file) {
            Ok(frecency) => (frecency, Some(file)),
            Err(why) => {
                let (then, file) = if why.is_damaged() {
                    ("starts again", Some(file))
                } else {
                    ("is left as it is", None)
                };
                let note = format!("the list of opened files could not be read and {then}: {why}");
                core.send(Job::Note(note));
                (Frecency::default(), file)
            }
        };
        let count = frecency.len() as u32;
        *lock(&core.live.opens) = Opens { frecency, file };
        core.send(Job::Opened);
        count
    })
}

/// Counts one open of `path`, a real path in UTF-8 of a file or folder the window opened, or an
/// app's bundle as its row names it, so it ranks higher from the next search on, which
/// `TL_EVENT_RERANKED` tells of. With a file from `tl_frecency_open`, the list is saved
/// there before this returns, so call it off the main thread; a save that fails is told as a
/// `TL_EVENT_NOTE`, and the open still counts until the core is freed. Returns 1 once counted,
/// or 0 for a path that is empty, isn't UTF-8 or is longer than 4096 bytes. Safe to call from
/// any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `path` points to `len` bytes
/// (or `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_opened(core: *const TlCore, path: *const u8, len: usize) -> u32 {
    guard(0, || {
        let core = unsafe { &*core };
        let bytes = match len {
            0 => return 0,
            _ => unsafe { std::slice::from_raw_parts(path, len) },
        };
        let Ok(path) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let _turn = lock(&core.live.saves);
        let mut opens = lock(&core.live.opens);
        if !opens.frecency.record(Path::new(path), unix_now()) {
            return 0;
        }
        let saving = opens
            .file
            .clone()
            .map(|file| (opens.frecency.clone(), file));
        drop(opens);
        core.send(Job::Opened);
        if let Some((frecency, file)) = saving
            && let Err(error) = frecency.save(&file)
        {
            let note = format!("the list of opened files could not be saved: {error}");
            core.send(Job::Note(note));
        }
        1
    })
}

/// The opened-files locks are taken past a panic `guard` caught, so one bad open never stops
/// every later open and the search worker with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Text {
    fn search(&self) -> &TextSearch {
        self.started.get_or_init(|| {
            let grep = Arc::new(Grep::default());
            let waiting = Arc::default();
            let (jobs, inbox) = mpsc::channel();
            let worker = {
                let (grep, waiting) = (Arc::clone(&grep), Arc::clone(&waiting));
                let (home, notify) = (self.home.clone(), self.notify);
                thread::Builder::new()
                    .name("text".into())
                    .spawn(move || {
                        find_loop(&grep, &inbox, &waiting, notify, |find| {
                            find_in(&grep, find, &waiting, &home, notify)
                        });
                    })
                    .expect("start the text search worker")
            };
            TextSearch {
                grep,
                jobs,
                waiting,
                worker,
            }
        })
    }
}

/// Runs each wanted search with `find_in`, which gives `None` once it is no longer wanted.
fn find_loop(
    grep: &Grep,
    inbox: &Receiver<Find>,
    waiting: &Mutex<Waiting>,
    notify: Notify,
    find_in: impl Fn(&Find) -> Option<Report>,
) {
    while let Ok(mut find) = inbox.recv() {
        // Keys pressed while the last search ran: only the newest one is still wanted, and
        // skipping the rest saves listing their folders.
        while let Ok(newer) = inbox.try_recv() {
            find = newer;
        }
        let number = find.ticket.number();
        {
            let mut waiting = waiting.lock().unwrap();
            if grep.newest() != number {
                continue;
            }
            *waiting = Waiting {
                ticket: number,
                ..Waiting::default()
            };
        }
        let report = panic::catch_unwind(AssertUnwindSafe(|| find_in(&find)));
        let mut waiting = waiting.lock().unwrap();
        if grep.newest() != number {
            continue;
        }
        // A bug ends this search alone, so the window stops waiting and can say why.
        let report = report.unwrap_or_else(|_| {
            waiting.failed = true;
            Some(Report {
                lines_found: waiting.found,
                ..Report::default()
            })
        });
        if let Some(report) = report {
            waiting.report = Some(report);
            tell(waiting, number, notify);
        }
    }
}

/// Lists each folder as the name list would, at the time of the search, then searches it, until
/// `LINES` lines in all. `None` once a newer search or a cancel took over.
fn find_in(
    grep: &Grep,
    find: &Find,
    waiting: &Mutex<Waiting>,
    home: &Path,
    notify: Notify,
) -> Option<Report> {
    if !argus_grep::can_search(&find.query) {
        return Some(Report::default());
    }
    let number = find.ticket.number();
    let stale = || grep.newest() != number;
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    // Problems with the list are told by the name list already.
    let (ignore, _) = Ignore::load(&home, find.access);
    let mut report = Report::default();
    for folder in outermost(&find.folders) {
        let limit = LINES.saturating_sub(report.lines_found);
        if limit == 0 || stale() {
            break;
        }
        // Listing a home folder takes all cores for about 0.1 s: on the text search threads, it
        // leaves name searches alone, and a newer keystroke stops it.
        let scan = grep.install(|| {
            argus_core::scan_until(&folder, &home, Clutter::Skip, &ignore, find.access, &stale)
        });
        if stale() {
            return None;
        }
        let Ok(scan) = scan else {
            report.unreadable += 1;
            continue;
        };
        let add = |found: Found<'_>| add(waiting, grep, number, &found, notify);
        report = report + grep.search(&scan.index, &find.query, find.ticket, limit, add)?;
    }
    (!stale()).then_some(report)
}

/// The folders as real paths, each once, leaving out any inside another.
fn outermost(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut real: Vec<PathBuf> = folders
        .iter()
        .map(|folder| std::fs::canonicalize(folder).unwrap_or_else(|_| folder.clone()))
        .collect();
    // A folder sorts before everything inside it.
    real.sort();
    let mut kept: Vec<PathBuf> = Vec::with_capacity(real.len());
    for folder in real {
        if !kept.iter().any(|outer| folder.starts_with(outer)) {
            kept.push(folder);
        }
    }
    kept
}

fn add(waiting: &Mutex<Waiting>, grep: &Grep, number: u64, found: &Found<'_>, notify: Notify) {
    let mut waiting = waiting.lock().unwrap();
    // Read just as a newer search or a cancel came in.
    if grep.newest() != number {
        return;
    }
    waiting.found += 1;
    let batch = &mut waiting.batch;
    let path_start = batch.text.len() as u32;
    batch.text.push_str(&found.path.to_string_lossy());
    let text_start = batch.text.len() as u32;
    batch.text.push_str(found.text);
    batch.lines.push(TlLine {
        path_start,
        path_len: text_start - path_start,
        text_start,
        text_len: found.text.len() as u32,
        match_start: found.matched.start as u32,
        match_len: found.matched.len() as u32,
        line: found.line,
    });
    tell(waiting, number, notify);
}

/// Says lines or the end are waiting, unless the window was told already and hasn't taken them.
fn tell(mut waiting: MutexGuard<'_, Waiting>, number: u64, notify: Notify) {
    let told = std::mem::replace(&mut waiting.told, true);
    drop(waiting);
    if !told {
        notify.send(TL_EVENT_LINES, number);
    }
}

/// Starts a search for `query` inside the files in `folders` and returns its ticket, or 0 on
/// failure, as for a folder that isn't UTF-8, leaving the search under way alone. Otherwise
/// every older text search gives up; name searches never wait for any. `folders` holds
/// `folders_len` bytes: paths in UTF-8, each ending in a NUL byte. Each is listed as the name
/// list would be, skipping clutter and the ignore list, and a folder inside another is
/// searched once; one that can't be listed counts as unreadable.
/// Upper-case letters in `query` must match exactly; lower-case ones match either case. Up to
/// 100 lines come in batches, each announced by `TL_EVENT_LINES`, the last with the end.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, `folders` points to `folders_len`
/// bytes (or `folders_len` is 0), and `query` to `query_len` bytes (or `query_len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grep_submit(
    core: *const TlCore,
    folders: *const u8,
    folders_len: usize,
    query: *const u8,
    query_len: usize,
) -> u64 {
    guard(0, || {
        let core = unsafe { &*core };
        let Some(text) = &core.text else {
            return 0;
        };
        let folders = match folders_len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(folders, folders_len) },
        };
        let folders: Result<Vec<PathBuf>, _> = folders
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| std::str::from_utf8(path).map(PathBuf::from))
            .collect();
        let Ok(folders) = folders else {
            return 0;
        };
        let query = match query_len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(query, query_len) },
        };
        let query = String::from_utf8_lossy(query).into_owned();
        let access = if text.full_disk.load(Ordering::Relaxed) {
            Access::Full
        } else {
            Access::Limited
        };
        let search = text.search();
        let ticket = search.grep.ticket();
        let find = Find {
            ticket,
            folders,
            query,
            access,
        };
        // Fails only if the worker stopped after a bug.
        if search.jobs.send(find).is_ok() {
            ticket.number()
        } else {
            0
        }
    })
}

/// Hands over the lines `ticket` found since the last take, and its end once it is over, while
/// no newer text search or cancel exists. `TL_GONE` when nothing is waiting. `out` is written
/// only when this returns `TL_OK`.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `out` points to a `TlLines` to
/// fill.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grep_take(core: *const TlCore, ticket: u64, out: *mut TlLines) -> u32 {
    guard(TL_PANICKED, || {
        let core = unsafe { &*core };
        let Some(search) = core.text.as_ref().and_then(|text| text.started.get()) else {
            return TL_GONE;
        };
        if ticket != search.grep.newest() {
            return TL_GONE;
        }
        let mut waiting = search.waiting.lock().unwrap();
        if waiting.ticket != ticket || !waiting.told {
            return TL_GONE;
        }
        waiting.told = false;
        let batch = std::mem::take(&mut waiting.batch);
        let report = waiting.report.take();
        let failed = waiting.failed;
        drop(waiting);
        let batch = Box::new(batch);
        let lines = TlLines {
            lines: batch.lines.as_ptr(),
            count: batch.lines.len(),
            text: batch.text.as_ptr(),
            done: report.is_some(),
            failed,
            report: report.unwrap_or_default().into(),
            owner: Box::into_raw(batch).cast(),
        };
        unsafe { out.write(lines) };
        TL_OK
    })
}

/// Stops the text search under way, if any, and drops what it found: no more lines and no end
/// come for it, though a `TL_EVENT_LINES` already on its way may still arrive.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grep_cancel(core: *const TlCore) {
    guard((), || {
        let core = unsafe { &*core };
        if let Some(search) = core.text.as_ref().and_then(|text| text.started.get()) {
            search.grep.ticket();
            *search.waiting.lock().unwrap() = Waiting::default();
        }
    })
}

/// # Safety
/// `lines` came from `tl_grep_take` and has not been freed yet, or is all zeros.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_lines_free(lines: TlLines) {
    guard((), || {
        if !lines.owner.is_null() {
            drop(unsafe { Box::from_raw(lines.owner.cast::<Batch>()) });
        }
    })
}

/// Stops the core and waits for its threads, so `on_event` never runs once this returns.
/// Called while a list is being built, it waits for the build to finish: under 0.2 s for a
/// million made-up names, but a home folder scanned with nothing cached may take longer. A
/// text search stops after the folder or file it is reading.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in` and is freed once, never from
/// inside `on_event`, and no other thread is still calling into it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_core_free(core: *mut TlCore) {
    guard((), || {
        if !core.is_null() {
            drop(unsafe { Box::from_raw(core) });
        }
    })
}

impl Drop for TlCore {
    fn drop(&mut self) {
        // A fresh ticket makes the search in flight give up.
        self.searcher.ticket();
        self.live.plan(|plan| plan.stopped = true);
        drop(self.jobs.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // A list still waiting goes unsaved: the next launch opens an older one and catches up.
        self.live.saving.lock().unwrap().stopped = true;
        self.live.save.notify_all();
        if let Some(saver) = self.saver.take() {
            let _ = saver.join();
        }
        if let Some(search) = self.text.as_mut().and_then(|text| text.started.take()) {
            let TextSearch {
                grep, jobs, worker, ..
            } = search;
            grep.ticket();
            drop(jobs);
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicPtr;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Events {
        seen: Mutex<Vec<(u32, u64)>>,
        notes: Mutex<Vec<String>>,
        noted: Condvar,
        changed: Condvar,
        /// While true, a text search thread telling of lines waits in `on_event`.
        hold_lines: Mutex<bool>,
        let_go: Condvar,
    }

    unsafe extern "C" fn record(ctx: *mut c_void, event: u32, value: u64) {
        let events = unsafe { &*(ctx as *const Events) };
        if event == TL_EVENT_NOTE {
            let text = unsafe { std::ffi::CStr::from_ptr(value as *const std::ffi::c_char) };
            events
                .notes
                .lock()
                .unwrap()
                .push(text.to_str().unwrap().to_owned());
            events.noted.notify_all();
            return;
        }
        events.seen.lock().unwrap().push((event, value));
        events.changed.notify_all();
        if event == TL_EVENT_LINES {
            let held = events.hold_lines.lock().unwrap();
            drop(events.let_go.wait_while(held, |held| *held).unwrap());
        }
    }

    impl Events {
        fn hold_lines(&self, hold: bool) {
            *self.hold_lines.lock().unwrap() = hold;
            self.let_go.notify_all();
        }

        fn count(&self, event: u32, value: u64) -> usize {
            let seen = self.seen.lock().unwrap();
            seen.iter().filter(|&&seen| seen == (event, value)).count()
        }

        /// Waits until `event` with `value` has come `times` times.
        fn wait_for_times(&self, event: u32, value: u64, times: usize) {
            let heard = self.heard_within(event, value, times, Duration::from_secs(60));
            assert!(heard, "event {event} for {value} not {times} times");
        }

        /// Whether `event` with `value` comes within 10 s, without failing the test, for a
        /// test that must let a held thread go first.
        fn heard(&self, event: u32, value: u64) -> bool {
            self.heard_within(event, value, 1, Duration::from_secs(10))
        }

        fn heard_within(&self, event: u32, value: u64, times: usize, wait: Duration) -> bool {
            let seen = self.seen.lock().unwrap();
            let (_seen, waited) = self
                .changed
                .wait_timeout_while(seen, wait, |seen| {
                    seen.iter().filter(|&&seen| seen == (event, value)).count() < times
                })
                .unwrap();
            !waited.timed_out()
        }

        fn wait_for(&self, event: u32, value: u64) {
            let seen = self.seen.lock().unwrap();
            let (_seen, waited) = self
                .changed
                .wait_timeout_while(seen, Duration::from_secs(60), |seen| {
                    !seen.contains(&(event, value))
                })
                .unwrap();
            assert!(!waited.timed_out(), "no event {event} for {value}");
        }

        fn wait_for_event(&self, event: u32) -> u64 {
            let seen = self.seen.lock().unwrap();
            let (seen, waited) = self
                .changed
                .wait_timeout_while(seen, Duration::from_secs(60), |seen| {
                    !seen.iter().any(|(e, _)| *e == event)
                })
                .unwrap();
            assert!(!waited.timed_out(), "no event {event}");
            seen.iter().find(|(e, _)| *e == event).unwrap().1
        }

        /// Waits for a note holding `part`, and returns every note so far.
        fn wait_for_note(&self, part: &str) -> Vec<String> {
            let notes = self.notes.lock().unwrap();
            let (notes, waited) = self
                .noted
                .wait_timeout_while(notes, Duration::from_secs(60), |notes| {
                    !notes.iter().any(|note| note.contains(part))
                })
                .unwrap();
            assert!(!waited.timed_out(), "no note with {part:?}: {notes:?}");
            notes.clone()
        }

        fn ready(&self) -> Vec<u64> {
            let seen = self.seen.lock().unwrap();
            seen.iter()
                .filter(|(event, _)| *event == TL_EVENT_READY)
                .map(|&(_, ticket)| ticket)
                .collect()
        }

        /// The entry count of each rebuilt list so far.
        fn rebuilt(&self) -> Vec<u64> {
            let seen = self.seen.lock().unwrap();
            seen.iter()
                .filter(|(event, _)| *event == TL_EVENT_REBUILT)
                .map(|&(_, value)| value & u64::from(u32::MAX))
                .collect()
        }

        /// Waits for the `nth` rebuilt list, counting from 1, and returns its entry count.
        fn wait_for_rebuilt(&self, nth: usize) -> u64 {
            let seen = self.seen.lock().unwrap();
            let (seen, waited) = self
                .changed
                .wait_timeout_while(seen, Duration::from_secs(60), |seen| {
                    seen.iter().filter(|(e, _)| *e == TL_EVENT_REBUILT).count() < nth
                })
                .unwrap();
            assert!(!waited.timed_out(), "no rebuild number {nth}");
            drop(seen);
            self.rebuilt()[nth - 1]
        }
    }

    /// Frees the core even when an assertion fails, before the `Events` it points to go away.
    struct Core(*mut TlCore);

    impl Drop for Core {
        fn drop(&mut self) {
            unsafe { tl_core_free(self.0) };
        }
    }

    fn start(count: u64, events: &Events) -> Core {
        let ctx = events as *const Events as *mut c_void;
        let core = unsafe { tl_core_new_stand_in(count, ctx, Some(record)) };
        assert!(!core.is_null());
        let core = Core(core);
        events.wait_for(TL_EVENT_INDEXED, count);
        core
    }

    fn submit(core: &Core, query: &str) -> u64 {
        unsafe { tl_submit(core.0, query.as_ptr(), query.len(), std::ptr::null(), 0) }
    }

    fn submit_in(core: &Core, query: &str, folder: &str) -> u64 {
        let (at, len) = (folder.as_ptr(), folder.len());
        unsafe { tl_submit(core.0, query.as_ptr(), query.len(), at, len) }
    }

    /// The names a search inside `folder` finds, once its rows are ready.
    fn found_in(core: &Core, events: &Events, query: &str, folder: &Path) -> Vec<String> {
        let ticket = submit_in(core, query, folder.to_str().unwrap());
        events.wait_for(TL_EVENT_READY, ticket);
        let rows = take(core, ticket).unwrap();
        rows.into_iter().map(|(name, ..)| name).collect()
    }

    fn take(core: &Core, ticket: u64) -> Option<Vec<(String, String, bool)>> {
        let mut rows = std::mem::MaybeUninit::<TlRows>::uninit();
        if unsafe { tl_take(core.0, ticket, rows.as_mut_ptr()) } != TL_OK {
            return None;
        }
        Some(copy(unsafe { rows.assume_init() }))
    }

    fn watched(core: &Core) -> (Vec<(String, bool)>, u64) {
        let mut rows = std::mem::MaybeUninit::<TlRows>::uninit();
        let mut since = u64::MAX;
        assert_eq!(
            unsafe { tl_watched(core.0, rows.as_mut_ptr(), &mut since) },
            TL_OK
        );
        let rows = copy(unsafe { rows.assume_init() });
        assert!(rows.iter().all(|(_, folder, _)| folder.is_empty()));
        let folders = rows
            .into_iter()
            .map(|(path, _, inside)| (path, inside))
            .collect();
        (folders, since)
    }

    fn watched_inside(core: &Core) -> Vec<String> {
        let (folders, _) = watched(core);
        let inside = folders.into_iter().filter(|(_, inside)| *inside);
        inside.map(|(path, _)| path).collect()
    }

    fn changed(core: &Core, paths: &[&Path], rescan: u32) -> u32 {
        changed_at(core, paths, rescan, 1)
    }

    fn changed_at(core: &Core, paths: &[&Path], rescan: u32, mark: u64) -> u32 {
        let mut bytes = Vec::new();
        for path in paths {
            bytes.extend_from_slice(path.to_str().unwrap().as_bytes());
            bytes.push(0);
        }
        unsafe { tl_changed(core.0, bytes.as_ptr(), bytes.len(), rescan, mark) }
    }

    fn copy(rows: TlRows) -> Vec<(String, String, bool)> {
        let copied = copy_all(rows).into_iter();
        copied
            .map(|row| (row.name, row.folder, row.is_folder))
            .collect()
    }

    #[derive(Debug, PartialEq)]
    struct Shown {
        name: String,
        folder: String,
        is_folder: bool,
        is_app: bool,
    }

    fn copy_all(rows: TlRows) -> Vec<Shown> {
        let text = |start: u32, len: u32| {
            let bytes =
                unsafe { std::slice::from_raw_parts(rows.text.add(start as usize), len as usize) };
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        let copied = unsafe { std::slice::from_raw_parts(rows.rows, rows.count) }
            .iter()
            .map(|row| Shown {
                name: text(row.name_start, row.name_len),
                folder: text(row.folder_start, row.folder_len),
                is_folder: row.is_folder,
                is_app: row.is_app,
            })
            .collect();
        unsafe { tl_rows_free(rows) };
        copied
    }

    #[test]
    fn a_search_comes_back_as_rows_once() {
        let events = Events::default();
        let core = start(2_000, &events);
        let ticket = submit(&core, "budget");
        events.wait_for(TL_EVENT_READY, ticket);

        let index = argus_core::stand_in(2_000);
        let searcher = Searcher::with_threads(1);
        let expected: Vec<_> = searcher
            .search(&index, &Apps::default(), "budget", searcher.ticket(), ROWS)
            .unwrap()
            .iter()
            .map(|hit| {
                let entry = hit.entry().unwrap();
                let path = index.path(entry);
                let folder = path.parent().unwrap().to_string_lossy().into_owned();
                (index.name(entry).to_owned(), folder, index.is_dir(entry))
            })
            .collect();
        assert_eq!(expected.len(), ROWS);
        assert_eq!(take(&core, ticket), Some(expected));
        assert_eq!(take(&core, ticket), None);
    }

    fn apps(core: &Core, folders: &[&Path]) -> u32 {
        let mut bytes = Vec::new();
        for folder in folders {
            bytes.extend_from_slice(folder.to_str().unwrap().as_bytes());
            bytes.push(0);
        }
        unsafe { tl_apps(core.0, bytes.as_ptr(), bytes.len()) }
    }

    fn shown(core: &Core, events: &Events, query: &str) -> Vec<Shown> {
        let ticket = submit(core, query);
        events.wait_for(TL_EVENT_READY, ticket);
        let mut rows = std::mem::MaybeUninit::<TlRows>::uninit();
        assert_eq!(unsafe { tl_take(core.0, ticket, rows.as_mut_ptr()) }, TL_OK);
        copy_all(unsafe { rows.assume_init() })
    }

    fn app(name: &str, folder: &Path) -> Shown {
        let folder = folder.to_str().unwrap().to_owned();
        let name = name.to_owned();
        Shown {
            name,
            folder,
            is_folder: false,
            is_app: true,
        }
    }

    fn app_folder(apps: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for app in apps {
            std::fs::create_dir_all(root.join(app)).unwrap();
        }
        (dir, root)
    }

    #[test]
    fn apps_are_found_by_name_beside_the_files() {
        let (_dir, root) = app_folder(&[
            "Budget.app/Contents",
            "Utilities/Budget Planner.app",
            "Utilities/Deeper/Budget Deep.app",
        ]);
        let events = Events::default();
        let core = start(2_000, &events);
        let files = shown(&core, &events, "budget");
        assert!(files.iter().all(|row| !row.is_app));

        assert_eq!(apps(&core, &[&root]), 1);
        let rows = shown(&core, &events, "budget");
        assert_eq!(rows.len(), ROWS);
        assert_eq!(rows[0], app("Budget", &root));
        assert!(rows.contains(&app("Budget Planner", &root.join("Utilities"))));
        assert_eq!(rows.iter().filter(|row| row.is_app).count(), 2, "{rows:?}");
        // The apps take the places of the weakest files.
        let rest: Vec<&Shown> = rows.iter().filter(|row| !row.is_app).collect();
        assert_eq!(rest, files.iter().take(ROWS - 2).collect::<Vec<_>>());
    }

    #[test]
    fn apps_read_again_follow_installs_and_deletes() {
        let (_dir, root) = app_folder(&["Budget.app"]);
        let events = Events::default();
        let core = start(2_000, &events);
        let found = |query| {
            let rows = shown(&core, &events, query);
            rows.into_iter()
                .filter(|row| row.is_app)
                .map(|row| row.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(apps(&core, &[&root]), 1);
        assert_eq!(found("budget"), ["Budget"]);

        std::fs::create_dir(root.join("Budget Tracker.app")).unwrap();
        std::fs::remove_dir(root.join("Budget.app")).unwrap();
        assert_eq!(found("budget"), ["Budget"], "until they are read again");
        assert_eq!(apps(&core, &[&root]), 1);
        assert_eq!(found("budget"), ["Budget Tracker"]);

        assert_eq!(unsafe { tl_apps(core.0, std::ptr::null(), 0) }, 1);
        assert!(found("budget").is_empty());
    }

    #[test]
    fn an_opened_app_ranks_first_and_stays_first_in_the_next_core() {
        let (_dir, root) = app_folder(&["Budget.app", "Budget Planner.app"]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let planner = root.join("Budget Planner.app");
        for open in [true, false] {
            let events = Events::default();
            let core = start(2_000, &events);
            assert_eq!(frecency_open(&core, &file), u32::from(!open));
            assert_eq!(apps(&core, &[&root]), 1);
            if open {
                assert_eq!(shown(&core, &events, "budget")[0], app("Budget", &root));
                assert_eq!(opened(&core, &planner), 1);
            }
            let rows = shown(&core, &events, "budget");
            assert_eq!(
                rows[..2],
                [app("Budget Planner", &root), app("Budget", &root)]
            );
        }
    }

    /// Apps live outside your folders, so a search inside one shows none.
    #[test]
    fn a_search_inside_a_folder_shows_no_apps() {
        let (_dir, root) = app_folder(&["Budget.app"]);
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(apps(&core, &[&root]), 1);
        let ticket = submit_in(&core, "budget", "/stand-in");
        events.wait_for(TL_EVENT_READY, ticket);
        let mut rows = std::mem::MaybeUninit::<TlRows>::uninit();
        assert_eq!(unsafe { tl_take(core.0, ticket, rows.as_mut_ptr()) }, TL_OK);
        let rows = copy_all(unsafe { rows.assume_init() });
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| !row.is_app), "{rows:?}");
    }

    #[test]
    fn folders_it_cannot_read_add_no_apps() {
        let (_dir, root) = app_folder(&["Budget.app"]);
        let events = Events::default();
        let core = start(2_000, &events);
        let mut bytes = b"\0\0".to_vec();
        bytes.extend_from_slice(&[b'/', 0xFF, 0]);
        bytes.extend_from_slice(b"/no/such/folder\0relative\0");
        // The last folder needs no NUL after it.
        bytes.extend_from_slice(root.to_str().unwrap().as_bytes());
        assert_eq!(unsafe { tl_apps(core.0, bytes.as_ptr(), bytes.len()) }, 1);
        let rows = shown(&core, &events, "budget");
        let found: Vec<&str> = rows
            .iter()
            .filter(|row| row.is_app)
            .map(|row| &*row.name)
            .collect();
        assert_eq!(found, ["Budget"]);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_stopped_core_takes_no_apps() {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let core = scan(dir.path().join("gone").to_str().unwrap(), 1, &events);
        events.wait_for_event(TL_EVENT_FAILED);
        // The worker is gone once it has told of the failure and returned.
        for _ in 0..500 {
            if apps(&core, &[dir.path()]) == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the core still takes apps");
    }

    /// A relative folder would be read from wherever the app was started, "/" for the window.
    #[cfg(unix)]
    #[test]
    fn a_relative_folder_adds_no_apps_even_where_it_leads_to_some() {
        let (_dir, root) = app_folder(&["Budget.app"]);
        let here = std::env::current_dir().unwrap();
        let up = "../".repeat(here.components().count() - 1);
        let relative = PathBuf::from(up).join(root.strip_prefix("/").unwrap());
        assert!(relative.join("Budget.app").is_dir());
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(apps(&core, &[&relative]), 1);
        let rows = shown(&core, &events, "budget");
        assert!(rows.iter().all(|row| !row.is_app), "{rows:?}");
    }

    #[test]
    fn reranked_is_told_when_the_apps_change_or_an_open_counts() {
        let (_dir, root) = app_folder(&["Budget.app"]);
        let events = Events::default();
        let core = start(2_000, &events);
        let reranked = || events.count(TL_EVENT_RERANKED, 0);
        assert_eq!(apps(&core, &[&root]), 1);
        events.wait_for_times(TL_EVENT_RERANKED, 0, 1);

        // The same apps read again: nothing to tell, and the search after it says so.
        assert_eq!(apps(&core, &[&root]), 1);
        shown(&core, &events, "budget");
        assert_eq!(reranked(), 1);

        std::fs::create_dir(root.join("Budget Tracker.app")).unwrap();
        assert_eq!(apps(&core, &[&root]), 1);
        events.wait_for_times(TL_EVENT_RERANKED, 0, 2);

        assert_eq!(opened(&core, &root.join("Budget.app")), 1);
        events.wait_for_times(TL_EVENT_RERANKED, 0, 3);
        shown(&core, &events, "budget");
        assert_eq!(reranked(), 3);
    }

    /// Apps are numbered by their place in the list read, which an install or delete shifts.
    #[test]
    fn an_opened_app_keeps_its_place_when_the_apps_read_again_move_it() {
        let (_dir, root) = app_folder(&["A Budget.app", "Budget.app", "Budget Planner.app"]);
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(opened(&core, &root.join("Budget Planner.app")), 1);
        let first_two = || -> Vec<String> {
            assert_eq!(apps(&core, &[&root]), 1);
            let rows = shown(&core, &events, "budget");
            rows.into_iter()
                .filter(|row| row.is_app)
                .map(|row| row.name)
                .take(2)
                .collect()
        };
        assert_eq!(first_two(), ["Budget Planner", "Budget"]);
        std::fs::remove_dir(root.join("A Budget.app")).unwrap();
        assert_eq!(first_two(), ["Budget Planner", "Budget"], "after a delete");
        std::fs::create_dir(root.join("Budget Aardvark.app")).unwrap();
        assert_eq!(
            first_two(),
            ["Budget Planner", "Budget"],
            "after an install"
        );
    }

    /// /Applications/Safari.app is a link, and the window counts the open at the row's path.
    #[cfg(unix)]
    #[test]
    fn an_app_reached_through_a_link_ranks_by_the_links_path() {
        let (_dir, root) = app_folder(&["Budget.app", "Apps"]);
        let (_far, elsewhere) = app_folder(&["Budget Planner.app"]);
        let apps_dir = root.join("Apps");
        std::os::unix::fs::symlink(root.join("Budget.app"), apps_dir.join("Budget.app")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.join("Budget Planner.app"),
            apps_dir.join("Budget Planner.app"),
        )
        .unwrap();
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(apps(&core, &[&apps_dir]), 1);
        let first = |core: &Core| shown(core, &events, "budget")[0].name.clone();
        assert_eq!(first(&core), "Budget");

        // Its own bundle, which no row shows, ranks nothing.
        assert_eq!(opened(&core, &elsewhere.join("Budget Planner.app")), 1);
        assert_eq!(first(&core), "Budget");
        assert_eq!(opened(&core, &apps_dir.join("Budget Planner.app")), 1);
        assert_eq!(first(&core), "Budget Planner");
    }

    #[test]
    fn the_last_of_several_searches_always_wins() {
        let events = Events::default();
        let core = start(2_000, &events);
        let tickets: Vec<u64> = [
            "b", "bu", "bud", "budg", "budge", "budget", "budget ", "budget n",
        ]
        .iter()
        .map(|typed| submit(&core, typed))
        .collect();
        let last = *tickets.last().unwrap();
        events.wait_for(TL_EVENT_READY, last);
        for &older in &tickets[..tickets.len() - 1] {
            assert_eq!(take(&core, older), None);
        }
        assert!(take(&core, last).is_some_and(|rows| !rows.is_empty()));
        assert!(events.ready().iter().all(|&ready| ready <= last));
    }

    #[test]
    fn finished_rows_are_gone_once_a_newer_search_starts() {
        let events = Events::default();
        let core = start(2_000, &events);
        let older = submit(&core, "bu");
        events.wait_for(TL_EVENT_READY, older);
        submit(&core, "bud");
        assert_eq!(take(&core, older), None);
    }

    #[test]
    fn an_empty_search_box_needs_no_pointer() {
        let events = Events::default();
        let core = start(100, &events);
        let ticket = unsafe { tl_submit(core.0, std::ptr::null(), 0, std::ptr::null(), 0) };
        events.wait_for(TL_EVENT_READY, ticket);
        assert_eq!(take(&core, ticket), Some(Vec::new()));
        let folder = "/stand-in";
        let ticket = unsafe { tl_submit(core.0, std::ptr::null(), 0, folder.as_ptr(), 6) };
        events.wait_for(TL_EVENT_READY, ticket);
        assert_eq!(take(&core, ticket), Some(Vec::new()));
    }

    #[test]
    fn a_search_inside_a_folder_comes_back_as_its_rows() {
        let events = Events::default();
        let core = start(20_000, &events);
        let index = argus_core::stand_in(20_000);
        let searcher = Searcher::with_threads(1);
        let apps = Apps::default();
        let mut checked = 0;
        for entry in (1..2_000).filter(|&entry| index.is_dir(entry)).step_by(97) {
            let folder = index.path(entry);
            let within = Some(Within::folder(&index, &folder).unwrap());
            for query in ["budget", "re", "notes md", "tax !pdf"] {
                let ticket = searcher.ticket();
                let expected: Vec<String> = searcher
                    .search_in(&index, &apps, within.as_ref(), query, ticket, ROWS)
                    .unwrap()
                    .iter()
                    .map(|hit| index.name(hit.entry().unwrap()).to_owned())
                    .collect();
                checked += expected.len();
                assert_eq!(
                    found_in(&core, &events, query, &folder),
                    expected,
                    "{folder:?}"
                );
            }
        }
        assert!(checked > 100, "{checked}");
        // Back out, everything is searched again.
        let folder = index.path((1..).find(|&entry| index.is_dir(entry)).unwrap());
        assert!(!found_in(&core, &events, "budget", &folder).is_empty());
        let ticket = submit(&core, "budget");
        events.wait_for(TL_EVENT_READY, ticket);
        let rows = take(&core, ticket).unwrap();
        assert_eq!(rows.len(), ROWS);
        let outside = |(_, at, _): &(String, String, bool)| !Path::new(at).starts_with(&folder);
        assert!(rows.iter().any(outside), "{folder:?}");
    }

    #[test]
    fn a_folder_the_list_does_not_hold_finds_nothing() {
        let events = Events::default();
        let core = start(2_000, &events);
        for folder in [
            "/stand-in/nowhere",
            "/elsewhere",
            "stand-in",
            "/stand-in/..",
        ] {
            assert!(found_in(&core, &events, "budget", Path::new(folder)).is_empty());
        }
        let bytes = [0xff, 0xfe];
        let ticket = unsafe { tl_submit(core.0, "budget".as_ptr(), 6, bytes.as_ptr(), 2) };
        events.wait_for(TL_EVENT_READY, ticket);
        assert_eq!(take(&core, ticket), Some(Vec::new()));
    }

    #[test]
    fn a_search_inside_a_folder_sees_the_new_list() {
        let (_dir, home) = home_with(&["notes/budget a.txt", "work/budget w.txt"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 5);
        let notes = home.join("notes");
        assert_eq!(found_in(&core, &events, "budget", &notes), ["budget a.txt"]);

        std::fs::write(notes.join("budget b.txt"), "").unwrap();
        assert_eq!(changed(&core, &[&notes], 0), 1);
        assert_eq!(events.wait_for_rebuilt(1), 6);
        let mut found = found_in(&core, &events, "budget", &notes);
        found.sort();
        assert_eq!(found, ["budget a.txt", "budget b.txt"]);

        std::fs::remove_dir_all(&notes).unwrap();
        assert_eq!(changed(&core, &[&home], 0), 1);
        assert_eq!(events.wait_for_rebuilt(2), 3);
        assert!(found_in(&core, &events, "budget", &notes).is_empty());
        let work = home.join("work");
        assert_eq!(found_in(&core, &events, "budget", &work), ["budget w.txt"]);

        // A folder gone and made again is found once the new list arrives.
        assert!(found_in(&core, &events, "budget", &notes).is_empty());
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("budget c.txt"), "").unwrap();
        assert_eq!(changed(&core, &[&home], 0), 1);
        assert_eq!(events.wait_for_rebuilt(3), 5);
        assert_eq!(found_in(&core, &events, "budget", &notes), ["budget c.txt"]);
    }

    fn scan(home: &str, full_disk: u32, events: &Events) -> Core {
        scan_keeping(home, full_disk, 0, events)
    }

    fn scan_keeping(home: &str, full_disk: u32, everything: u32, events: &Events) -> Core {
        scan_saving(home, full_disk, everything, std::ptr::null(), events)
    }

    fn scan_saving(
        home: &str,
        full_disk: u32,
        everything: u32,
        saved: *const TlSaved,
        events: &Events,
    ) -> Core {
        let ctx = events as *const Events as *mut c_void;
        let (root, len) = (home.as_ptr(), home.len());
        let core =
            unsafe { tl_core_new(root, len, full_disk, everything, saved, ctx, Some(record)) };
        assert!(!core.is_null());
        Core(core)
    }

    #[test]
    fn a_scanned_folder_comes_back_as_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("notes/node_modules")).unwrap();
        std::fs::write(root.join("notes/budget 2026.txt"), "").unwrap();
        std::fs::write(root.join("notes/node_modules/budget.js"), "").unwrap();

        let events = Events::default();
        let core = scan(root.to_str().unwrap(), 1, &events);
        // The folder itself, notes and the budget: node_modules is clutter.
        events.wait_for(TL_EVENT_INDEXED, 3);
        let ticket = submit(&core, "budget");
        events.wait_for(TL_EVENT_READY, ticket);
        let notes = root.join("notes").to_string_lossy().into_owned();
        assert_eq!(
            take(&core, ticket),
            Some(vec![("budget 2026.txt".to_owned(), notes, false)])
        );
    }

    #[test]
    fn without_full_disk_access_documents_stay_unread() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(home.join("Documents")).unwrap();
        std::fs::write(home.join("Documents/budget.pdf"), "").unwrap();

        // The home folder, Documents and the budget; then the home folder alone.
        for (full_disk, entries) in [(1, 3), (0, 1)] {
            let events = Events::default();
            let _core = scan(home.to_str().unwrap(), full_disk, &events);
            events.wait_for(TL_EVENT_INDEXED, entries);
        }
    }

    #[test]
    fn a_folder_that_cannot_be_scanned_stops_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "").unwrap();
        // A missing folder and a file each fail with the system's own number, never 0.
        for root in [dir.path().join("gone"), file] {
            let code = std::fs::read_dir(&root)
                .unwrap_err()
                .raw_os_error()
                .unwrap() as u64;
            let events = Events::default();
            let _core = scan(root.to_str().unwrap(), 1, &events);
            let failed = events.wait_for_event(TL_EVENT_FAILED);
            assert_ne!(failed, 0);
            // Windows reports a missing folder through a different call than read_dir.
            if cfg!(unix) {
                assert_eq!(failed, code);
            }
            assert_eq!(*events.seen.lock().unwrap(), [(TL_EVENT_FAILED, failed)]);
        }
    }

    #[test]
    fn the_ignore_list_is_read_again_and_each_problem_told_once() {
        let (_dir, home) = home_with(&["work/old/a.txt", "work/new/b.txt"]);
        let list = home.join(".config/argus/ignore");
        std::fs::create_dir_all(list.parent().unwrap()).unwrap();
        std::fs::write(&list, "work/old\n").unwrap();

        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        // The home folder, work, both folders in it and their files.
        events.wait_for(TL_EVENT_INDEXED, 6);
        let notes = || events.notes.lock().unwrap().clone();
        assert_eq!(notes().len(), 1);
        assert!(notes()[0].contains("line 1"), "{}", notes()[0]);

        assert_eq!(changed(&core, &[], 1), 1);
        assert_eq!(events.wait_for_rebuilt(1), 6);
        assert_eq!(notes().len(), 1, "the same problem again");

        // Saving the list is a change of its own, and its right line counts at once.
        std::fs::write(&list, "old\n~\n").unwrap();
        assert_eq!(changed(&core, &[list.parent().unwrap()], 0), 1);
        assert_eq!(events.wait_for_rebuilt(2), 4);
        assert_eq!(notes().len(), 2);
        assert!(notes()[1].contains("line 2"), "{}", notes()[1]);
    }

    #[test]
    fn a_core_that_failed_wants_no_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let events = Events::default();
        let core = scan(gone.to_str().unwrap(), 1, &events);
        events.wait_for_event(TL_EVENT_FAILED);
        let stopped = TlHealth {
            stopped: true,
            ..TlHealth::default()
        };
        assert_eq!(health(&core), stopped, "no list, and none coming");
        assert_eq!(changed(&core, &[], 1), 0);
        assert_eq!(health(&core), stopped, "Rebuild does nothing");
        unsafe { tl_grant_full_disk(core.0, 0, 1) };
        assert_eq!(changed(&core, &[&gone], 0), 0);
        assert_eq!(watched(&core), (Vec::new(), 0));
        drop(core);
        assert_eq!(events.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bug_while_searching_still_stops_the_rebuild_thread() {
        let events = Events::default();
        let notify = Notify {
            ctx: &events as *const Events as *mut c_void,
            on_event: record,
        };
        // A lock left poisoned by an earlier panic makes the next answer panic.
        let done: Arc<Done> = Arc::default();
        let poisoned = Arc::clone(&done);
        let _ = thread::spawn(move || {
            let _held = poisoned.lock().unwrap();
            panic!("a bug in the engine");
        })
        .join();
        let live = Arc::new(Live::default());
        let searcher = Searcher::with_threads(1);
        let (jobs, inbox) = mpsc::channel();
        let ticket = searcher.ticket();
        let query = "budget".to_owned();
        let folder = None;
        jobs.send(Job::Search {
            ticket,
            query,
            folder,
        })
        .unwrap();

        work(
            Source::StandIn(2_000),
            &searcher,
            &inbox,
            jobs.clone(),
            &done,
            &live,
            notify,
        );
        assert_eq!(Arc::strong_count(&live), 1, "the rebuild thread is gone");
        assert!(events.seen.lock().unwrap().contains(&(TL_EVENT_FAILED, 0)));
    }

    #[test]
    fn a_core_refuses_what_it_cannot_serve() {
        let nowhere = unsafe { tl_core_new_stand_in(100, std::ptr::null_mut(), None) };
        assert!(nowhere.is_null());
        let events = Events::default();
        let ctx = &events as *const Events as *mut c_void;
        let too_many = unsafe { tl_core_new_stand_in(u64::from(u32::MAX), ctx, Some(record)) };
        assert!(too_many.is_null());
        let none = std::ptr::null();
        let not_utf8 = unsafe { tl_core_new([0xff].as_ptr(), 1, 1, 0, none, ctx, Some(record)) };
        assert!(not_utf8.is_null());
        let no_root = unsafe { tl_core_new(std::ptr::null(), 0, 1, 0, none, ctx, Some(record)) };
        assert!(no_root.is_null());

        let (_dir, home) = home_with(&[]);
        let home = home.to_str().unwrap();
        let file = "/tmp/index";
        let bad = [0xff];
        for (file, journal) in [
            (&bad[..], &b"5F2A"[..]),
            (b"", b"5F2A"),
            (file.as_bytes(), &bad),
        ] {
            let saved = saved_at(file, journal, 1);
            let refused =
                unsafe { tl_core_new(home.as_ptr(), home.len(), 1, 0, &saved, ctx, Some(record)) };
            assert!(refused.is_null(), "{file:?} {journal:?}");
        }
        let missing = TlSaved {
            file: std::ptr::null(),
            ..saved_at(file.as_bytes(), b"", 1)
        };
        let no_journal = TlSaved {
            journal: std::ptr::null(),
            ..saved_at(file.as_bytes(), b"5F2A", 1)
        };
        for saved in [missing, no_journal] {
            let refused =
                unsafe { tl_core_new(home.as_ptr(), home.len(), 1, 0, &saved, ctx, Some(record)) };
            assert!(refused.is_null());
        }
    }

    /// Where a core saves its list; an empty `journal` may come without a pointer.
    /// The memory macOS counts against this process, as `footprint` shows it.
    #[cfg(target_os = "macos")]
    fn footprint() -> u64 {
        #[repr(C)]
        #[derive(Default)]
        struct RusageInfoV0 {
            uuid: [u8; 16],
            times_and_wakeups: [u64; 6],
            resident_size: u64,
            phys_footprint: u64,
            start_and_exit: [u64; 2],
        }
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV0) -> i32;
        }
        let mut info = RusageInfoV0::default();
        assert_eq!(
            unsafe { proc_pid_rusage(std::process::id() as i32, 0, &mut info) },
            0
        );
        info.phys_footprint
    }

    /// Run alone: `cargo test -p argus-ffi --release update_peak -- --ignored --nocapture`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "indexes the whole home folder with everything on, and needs Full Disk Access"]
    fn update_peak_with_everything_on() {
        let home = argus_core::home();
        let scratch = home.join("Library/Caches/argus-measure");
        std::fs::create_dir_all(&scratch).unwrap();
        let saves = tempfile::tempdir().unwrap();
        let file = saves.path().join("index").to_string_lossy().into_owned();
        let saved = saved_at(file.as_bytes(), b"", 1);
        let events = Events::default();
        let core = scan_saving(home.to_str().unwrap(), 1, 1, &saved, &events);
        events.wait_for_event(TL_EVENT_INDEXED);
        thread::sleep(Duration::from_secs(3));
        let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
        eprintln!("MEASURE idle after the scan {:.1} MB", mb(footprint()));
        for round in 1..=6_u64 {
            std::fs::write(scratch.join(format!("{round}.txt")), "").unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let sampler = thread::spawn({
                let stop = Arc::clone(&stop);
                move || {
                    let mut peak = 0;
                    while !stop.load(Ordering::Relaxed) {
                        peak = peak.max(footprint());
                        thread::sleep(Duration::from_millis(1));
                    }
                    peak
                }
            });
            let started = Instant::now();
            assert_eq!(changed_at(&core, &[&scratch], 0, round + 1), 1);
            let entries = events.wait_for_rebuilt(round as usize);
            let took = started.elapsed();
            stop.store(true, Ordering::Relaxed);
            let peak = sampler.join().unwrap();
            eprintln!(
                "MEASURE round {round}: peak {:.1} MB, list in use after {took:?}, {entries} entries",
                mb(peak)
            );
            thread::sleep(Duration::from_secs(2));
        }
        eprintln!("MEASURE idle at the end {:.1} MB", mb(footprint()));
        std::fs::remove_dir_all(scratch).unwrap();
    }

    /// How long a search sent while the list is patched waits. Run alone:
    /// `cargo test -p argus-ffi --release search_stall -- --ignored --nocapture`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "indexes the whole home folder with everything on, and needs Full Disk Access"]
    fn search_stall_with_everything_on() {
        let home = argus_core::home();
        let scratch = home.join("Library/Caches/argus-measure");
        std::fs::create_dir_all(&scratch).unwrap();
        let events = Events::default();
        let core = scan_keeping(home.to_str().unwrap(), 1, 1, &events);
        events.wait_for_event(TL_EVENT_INDEXED);
        let search = || {
            let started = Instant::now();
            let ticket = submit(&core, "budget");
            events.wait_for(TL_EVENT_READY, ticket);
            started.elapsed()
        };
        let mut alone: Vec<Duration> = (0..21).map(|_| search()).collect();
        alone.sort();
        eprintln!(
            "MEASURE a search alone: {:?} typical, {:?} at most",
            alone[10], alone[20]
        );
        for round in 1..=6_u64 {
            std::fs::write(scratch.join(format!("{round}.txt")), "").unwrap();
            let started = Instant::now();
            assert_eq!(changed_at(&core, &[&scratch], 0, round + 1), 1);
            let (mut longest, mut searches) = (Duration::ZERO, 0);
            while events.rebuilt().len() < round as usize {
                longest = longest.max(search());
                searches += 1;
            }
            eprintln!(
                "MEASURE round {round}: patched in {:?}, longest of {searches} searches meanwhile {longest:?}",
                started.elapsed()
            );
            thread::sleep(Duration::from_secs(1));
        }
        std::fs::remove_dir_all(scratch).unwrap();
    }

    fn saved_at(file: &[u8], journal: &[u8], now: u64) -> TlSaved {
        TlSaved {
            file: file.as_ptr(),
            file_len: file.len(),
            journal: if journal.is_empty() {
                std::ptr::null()
            } else {
                journal.as_ptr()
            },
            journal_len: journal.len(),
            now,
        }
    }

    #[test]
    fn both_kinds_of_core_space_rebuilds_by_the_gap() {
        let (_dir, home) = home_with(&[]);
        let (names, folder) = (Events::default(), Events::default());
        let cores = [start(100, &names), scan(home.to_str().unwrap(), 1, &folder)];
        for core in &cores {
            assert_eq!(unsafe { &(*core.0).live }.gap, GAP);
        }
    }

    #[test]
    fn freeing_the_core_silences_it() {
        let events = Events::default();
        let core = start(2_000, &events);
        submit(&core, "budget");
        drop(core);
        let heard = events.seen.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(events.seen.lock().unwrap().len(), heard);
    }

    fn home_with(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        for file in files {
            let path = home.join(file);
            if file.ends_with('/') {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "").unwrap();
            }
        }
        (dir, home)
    }

    #[test]
    fn a_change_in_a_listed_folder_brings_in_the_new_file() {
        let (_dir, home) = home_with(&["notes/a.txt", "notes/node_modules/"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let clutter = home.join("notes/node_modules");
        std::fs::write(clutter.join("budget.js"), "").unwrap();
        assert_eq!(changed(&core, &[&clutter], 0), 0);
        assert_eq!(changed(&core, &[&home.join("Library/Caches")], 0), 0);

        std::fs::write(home.join("notes/budget.txt"), "").unwrap();
        assert_eq!(changed(&core, &[&clutter, &home.join("notes")], 0), 1);
        assert_eq!(events.wait_for_rebuilt(1), 4);
        let ticket = submit(&core, "budget");
        events.wait_for(TL_EVENT_READY, ticket);
        let found = take(&core, ticket).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "budget.txt");

        // Nothing more changed, so once the rebuild ends none more is asked for.
        let live = unsafe { &(*core.0).live };
        let asked = Instant::now();
        while live.plan.lock().unwrap().running {
            assert!(
                asked.elapsed() < Duration::from_secs(60),
                "the rebuild never ended"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let plan = live.plan.lock().unwrap();
        assert!(!plan.wanted && plan.recheck.is_none());
        assert_eq!(events.rebuilt().len(), 1);
    }

    #[test]
    fn the_rebuild_time_rides_in_the_high_half() {
        let events = Events::default();
        // Enough names to take a few milliseconds to build.
        let core = start(200_000, &events);
        let asked = Instant::now();
        assert_eq!(changed(&core, &[], 1), 1);
        events.wait_for_rebuilt(1);
        let waited = asked.elapsed().as_millis();
        let seen = events.seen.lock().unwrap();
        let &(_, value) = seen.iter().find(|(e, _)| *e == TL_EVENT_REBUILT).unwrap();
        assert_eq!(value & u64::from(u32::MAX), 200_000);
        let took = u128::from(value >> 32);
        assert!(took >= 1 && took <= waited, "{took} ms of {waited}");
    }

    fn health(core: &Core) -> TlHealth {
        let mut out = TlHealth {
            entries: u64::MAX,
            ..TlHealth::default()
        };
        assert_eq!(unsafe { tl_health(core.0, &mut out) }, TL_OK);
        out
    }

    #[test]
    fn health_tells_the_first_scan_then_each_change_and_rebuild() {
        let (_dir, home) = home_with(&["notes/a.txt"]);
        let before = unix_now();
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let first = health(&core);
        assert_eq!(first.entries, 3);
        assert!(
            (before..=unix_now()).contains(&first.scanned_at),
            "{first:?}"
        );
        assert_eq!(first.changed_at, 0, "nothing changed yet");
        assert!(!first.busy);

        // Read again in place: a change, not a scan.
        std::fs::write(home.join("notes/b.txt"), "").unwrap();
        assert_eq!(changed(&core, &[&home.join("notes")], 0), 1);
        events.wait_for_rebuilt(1);
        let patched = health(&core);
        assert_eq!(patched.entries, 4);
        assert_eq!(
            (patched.scanned_at, patched.scan_ms),
            (first.scanned_at, first.scan_ms)
        );
        assert!((first.scanned_at..=unix_now()).contains(&patched.changed_at));

        // A whole new list, as the window's Rebuild button asks for one. Times are whole
        // seconds, so they are cleared first to see the rebuild set them.
        {
            let mut held = lock(&unsafe { &(*core.0).live }.health);
            (held.scanned_at, held.changed_at) = (0, 0);
        }
        assert_eq!(unsafe { tl_changed(core.0, std::ptr::null(), 0, 1, 0) }, 1);
        events.wait_for_rebuilt(2);
        let rebuilt = health(&core);
        assert_eq!(rebuilt.entries, 4);
        assert!(
            (first.scanned_at..=unix_now()).contains(&rebuilt.scanned_at),
            "{rebuilt:?}"
        );
        assert!(
            (first.scanned_at..=unix_now()).contains(&rebuilt.changed_at),
            "{rebuilt:?}"
        );
        assert!(!rebuilt.behind && !rebuilt.stopped);
    }

    /// The window reads the health as soon as it hears of a new list, while the rebuild
    /// thread may still be checking late changes.
    #[test]
    fn health_is_not_busy_by_the_time_the_window_hears_of_a_new_list() {
        struct Heard {
            core: AtomicPtr<TlCore>,
            busy: Mutex<Vec<bool>>,
        }
        unsafe extern "C" fn on_event(ctx: *mut c_void, event: u32, _: u64) {
            let heard = unsafe { &*(ctx as *const Heard) };
            let core = heard.core.load(Ordering::Acquire);
            if event == TL_EVENT_REBUILT && !core.is_null() {
                let mut out = TlHealth::default();
                unsafe { tl_health(core, &mut out) };
                heard.busy.lock().unwrap().push(out.busy);
            }
        }
        let heard = Heard {
            core: AtomicPtr::default(),
            busy: Mutex::default(),
        };
        let ctx = &heard as *const Heard as *mut c_void;
        let live = Live {
            gap: Duration::ZERO,
            ..Live::default()
        };
        let core = Core(super::start(
            ctx,
            Some(on_event),
            Source::StandIn(100),
            false,
            None,
            live,
        ));
        heard.core.store(core.0, Ordering::Release);
        wait_until("the first list", || health(&core).entries == 100);
        // The gap is a few microseconds, so it takes many rounds to land in it.
        for rebuilds in 1..=100 {
            assert_eq!(changed(&core, &[], 1), 1);
            wait_until("the rebuild heard", || {
                heard.busy.lock().unwrap().len() == rebuilds
            });
            wait_until("the rebuild to end", || {
                !unsafe { &(*core.0).live }.plan.lock().unwrap().running
            });
        }
        assert_eq!(*heard.busy.lock().unwrap(), [false; 100]);
    }

    fn holds(core: &Core, path: &Path) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes();
        unsafe { tl_holds_folder(core.0, bytes.as_ptr(), bytes.len()) == 1 }
    }

    #[test]
    fn only_folders_the_newest_list_holds_are_held() {
        let (_dir, home) = home_with(&["notes/a.txt", "Library/Caches/com.example/cache.db"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for_event(TL_EVENT_INDEXED);
        assert!(holds(&core, &home));
        assert!(holds(&core, &home.join("notes")));
        for left_out in [
            home.join("notes/a.txt"),
            home.join("Library/Caches"),
            home.join("gone"),
            home.with_extension("beside"),
            PathBuf::from("/"),
            PathBuf::from("notes"),
            PathBuf::new(),
        ] {
            assert!(!holds(&core, &left_out), "{left_out:?}");
        }
        assert_eq!(
            unsafe { tl_holds_folder(core.0, b"\xff".as_ptr(), 1) },
            0,
            "not UTF-8"
        );
        assert_eq!(unsafe { tl_holds_folder(core.0, std::ptr::null(), 0) }, 0);

        // A folder made since comes in with the next list.
        std::fs::create_dir(home.join("new")).unwrap();
        assert!(!holds(&core, &home.join("new")));
        assert_eq!(changed(&core, &[&home], 0), 1);
        events.wait_for_rebuilt(1);
        assert!(holds(&core, &home.join("new")));

        let events = Events::default();
        let ctx = &events as *const Events as *mut c_void;
        let made_up = Core(unsafe { tl_core_new_stand_in(100, ctx, Some(record)) });
        events.wait_for(TL_EVENT_INDEXED, 100);
        assert!(
            !holds(&made_up, Path::new("/")),
            "made-up names have no folders on disk"
        );
    }

    #[test]
    fn health_times_the_scan_and_is_busy_until_the_first_list() {
        let events = Events::default();
        let ctx = &events as *const Events as *mut c_void;
        // Enough names to take a few milliseconds to build.
        let asked = Instant::now();
        let core = Core(unsafe { tl_core_new_stand_in(500_000, ctx, Some(record)) });
        let early = health(&core);
        assert!(early.busy, "{early:?}");
        assert_eq!((early.entries, early.scanned_at), (0, 0));
        events.wait_for(TL_EVENT_INDEXED, 500_000);
        let waited = asked.elapsed().as_millis() as u64;
        let ready = health(&core);
        assert!(!ready.busy);
        assert_eq!(ready.entries, 500_000);
        assert!(
            ready.scan_ms >= 1 && ready.scan_ms <= waited,
            "{ready:?} of {waited}"
        );
    }

    #[test]
    fn health_stays_busy_while_a_rebuild_waits_out_the_gap() {
        let events = Events::default();
        let ctx = &events as *const Events as *mut c_void;
        let live = Live {
            gap: Duration::from_secs(3600),
            ..Live::default()
        };
        let source = Source::StandIn(100);
        let core = Core(super::start(ctx, Some(record), source, false, None, live));
        events.wait_for(TL_EVENT_INDEXED, 100);
        assert!(!health(&core).busy);
        assert_eq!(changed(&core, &[], 1), 1);
        assert!(health(&core).busy, "waits an hour after the first list");
    }

    #[test]
    fn only_dropped_changes_rebuild_made_up_names() {
        let events = Events::default();
        let core = start(100, &events);
        assert_eq!(changed(&core, &[], 0), 0);
        assert_eq!(changed(&core, &[Path::new("/Users")], 0), 0);
        assert_eq!(unsafe { tl_changed(core.0, std::ptr::null(), 0, 0, 1) }, 0);
        assert_eq!(watched(&core), (Vec::new(), 0));
        assert_eq!(unsafe { tl_changed(core.0, std::ptr::null(), 0, 1, 1) }, 1);
        assert_eq!(events.wait_for_rebuilt(1), 100);
        // Access changes nothing for them, but costs no more than one more rebuild.
        unsafe { tl_grant_full_disk(core.0, 0, 1) };
        assert_eq!(events.wait_for_rebuilt(2), 100);
    }

    #[test]
    fn a_path_that_is_not_utf8_counts_as_a_change() {
        let (_dir, home) = home_with(&["notes/"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 2);
        let bytes = [0xff, 0];
        assert_eq!(unsafe { tl_changed(core.0, bytes.as_ptr(), 2, 0, 1) }, 1);
        events.wait_for_rebuilt(1);
    }

    #[test]
    fn a_change_while_one_is_waiting_needs_no_check() {
        // No threads, so the rebuild asked for stays waiting.
        let core = Core(Box::into_raw(Box::new(TlCore {
            searcher: Arc::default(),
            jobs: None,
            done: Arc::default(),
            live: Arc::default(),
            worker: None,
            saver: None,
            saved_in: None,
            text: None,
            judge: None,
        })));
        let live = unsafe { &(*core.0).live };
        live.plan(|plan| plan.want(5));
        assert_eq!(changed_at(&core, &[Path::new("/elsewhere")], 0, 7), 1);
        assert_eq!(live.plan.lock().unwrap().asked, Some(5));
        assert_eq!(changed_at(&core, &[Path::new("/elsewhere")], 0, 3), 1);
        assert_eq!(live.plan.lock().unwrap().asked, Some(3));
        // A change FSEvents didn't report counts as the newest one heard.
        assert_eq!(live.plan.lock().unwrap().delivered, 7);
        assert_eq!(changed_at(&core, &[Path::new("/elsewhere")], 0, 0), 1);
        assert_eq!(live.plan.lock().unwrap().asked, Some(3));
        assert_eq!(live.plan.lock().unwrap().delivered, 7);
    }

    #[test]
    fn the_watched_folders_follow_the_newest_list() {
        let (_dir, home) = home_with(&["notes/", ".git/", "Library/Caches/"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let path = |name: &str| home.join(name).to_string_lossy().into_owned();
        let home_names = (home.to_string_lossy().into_owned(), false);
        let first = vec![
            home_names.clone(),
            (path(".config/argus"), true),
            (path("Library"), false),
            (path("notes"), true),
        ];
        assert_eq!(watched(&core), (first.clone(), 0));

        std::fs::create_dir(home.join("work")).unwrap();
        assert_eq!(changed_at(&core, &[&home], 0, 42), 1);
        events.wait_for_rebuilt(1);
        let mut second = first;
        second.push((path("work"), true));
        assert_eq!(watched(&core), (second, 42));
    }

    #[test]
    fn granting_full_disk_access_brings_in_the_guarded_folders() {
        let (_dir, home) = home_with(&["Documents/budget.pdf"]);
        let documents = home.join("Documents");
        let list = home.join(".config/argus").to_string_lossy().into_owned();
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 0, &events);
        events.wait_for(TL_EVENT_INDEXED, 1);
        assert_eq!(changed(&core, &[&documents], 0), 0);
        assert_eq!(watched_inside(&core), std::slice::from_ref(&list));

        unsafe { tl_grant_full_disk(core.0, 0, 9) };
        assert_eq!(events.wait_for_rebuilt(1), 3);
        let documents_text = documents.to_string_lossy().into_owned();
        assert_eq!(watched_inside(&core), [list, documents_text]);
        assert_eq!(watched(&core).1, 9);
        std::fs::write(documents.join("taxes.pdf"), "").unwrap();
        assert_eq!(changed(&core, &[&documents], 0), 1);
    }

    /// A home folder whose ignore list skips notes, with clutter in Library.
    fn home_with_clutter() -> (tempfile::TempDir, PathBuf) {
        let (dir, home) = home_with(&[
            "notes/a.txt",
            "Library/Caches/com.example/cache.db",
            ".config/argus/ignore",
        ]);
        std::fs::write(home.join(".config/argus/ignore"), "notes\n").unwrap();
        (dir, home)
    }

    /// The names a search finds.
    fn found(core: &Core, events: &Events, query: &str) -> Vec<String> {
        let ticket = submit(core, query);
        events.wait_for(TL_EVENT_READY, ticket);
        let rows = take(core, ticket).unwrap();
        rows.into_iter().map(|(name, ..)| name).collect()
    }

    /// Whether a folder watched inside covers `path`.
    fn watched_within(core: &Core, path: &Path) -> bool {
        watched_inside(core)
            .iter()
            .any(|folder| path.starts_with(folder))
    }

    #[test]
    fn indexing_everything_turns_on_and_off_without_a_new_core() {
        let (_dir, home) = home_with_clutter();
        let caches = home.join("Library/Caches/com.example");
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        // The home folder, and Library read only on the way to its cloud drives.
        events.wait_for(TL_EVENT_INDEXED, 2);
        assert!(found(&core, &events, "cache.db").is_empty());
        assert!(!watched_within(&core, &caches));

        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(switch(&core, 7, 4), 0, "already on");
        assert_eq!(events.wait_for_rebuilt(1), 10);
        assert_eq!(found(&core, &events, "cache.db"), ["cache.db"]);
        assert_eq!(found(&core, &events, "a.txt"), ["a.txt"]);
        assert_eq!(watched(&core).1, 3);
        assert!(watched_within(&core, &caches));
        // Later changes count by the new list's rules.
        std::fs::write(caches.join("new.db"), "").unwrap();
        assert_eq!(changed_at(&core, &[&caches], 0, 4), 1);
        assert_eq!(events.wait_for_rebuilt(2), 11);
        assert_eq!(found(&core, &events, "new.db"), ["new.db"]);

        assert_eq!(switch(&core, 0, 5), 1);
        assert_eq!(events.wait_for_rebuilt(3), 2);
        assert!(found(&core, &events, "cache.db").is_empty());
        assert!(!watched_within(&core, &caches));
        std::fs::write(caches.join("newer.db"), "").unwrap();
        assert_eq!(changed(&core, &[&caches], 0), 0);
        assert_eq!(switch(&core, 0, 6), 0, "already off");

        // The next rebuild is the one asked for here, so none came in between.
        assert_eq!(changed_at(&core, &[], 1, 7), 1);
        assert_eq!(events.wait_for_rebuilt(4), 2);
        assert_eq!(events.rebuilt().len(), 4);
        assert_eq!(watched(&core).1, 7);
    }

    #[test]
    fn a_core_started_keeping_everything_scans_once_and_skips_the_ignore_list() {
        let (_dir, home) = home_with_clutter();
        std::fs::write(home.join(".config/argus/ignore"), "work/old\n").unwrap();
        let events = Events::default();
        let core = scan_keeping(home.to_str().unwrap(), 1, 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 10);
        assert_eq!(found(&core, &events, "cache.db"), ["cache.db"]);
        assert!(events.notes.lock().unwrap().is_empty());
        assert_eq!(changed_at(&core, &[], 1, 7), 1);
        assert_eq!(events.wait_for_rebuilt(1), 10);
        assert_eq!(watched(&core).1, 7);
        assert!(events.notes.lock().unwrap().is_empty());

        // The list counts again once clutter is skipped.
        assert_eq!(switch(&core, 0, 8), 1);
        events.wait_for_rebuilt(2);
        assert_eq!(events.notes.lock().unwrap().len(), 1);
        assert!(found(&core, &events, "cache.db").is_empty());
    }

    #[test]
    fn a_switch_during_the_first_scan_reaches_a_list() {
        let (_dir, home) = home_with_clutter();
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        assert_eq!(switch(&core, 1, 3), 1);
        // The first list keeps clutter if it started after the switch; else a rebuild does.
        if events.wait_for_event(TL_EVENT_INDEXED) != 10 {
            assert_eq!(events.wait_for_rebuilt(1), 10);
        }
        assert_eq!(found(&core, &events, "cache.db"), ["cache.db"]);
    }

    #[test]
    fn switching_made_up_names_builds_them_again() {
        let events = Events::default();
        let core = start(100, &events);
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(events.wait_for_rebuilt(1), 100);
    }

    #[test]
    fn a_stopped_core_ignores_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let core = scan(dir.path().join("gone").to_str().unwrap(), 1, &events);
        events.wait_for_event(TL_EVENT_FAILED);
        assert_eq!(switch(&core, 1, 1), 0);
        drop(core);
        assert_eq!(events.seen.lock().unwrap().len(), 1);
    }

    /// Takes any folder as a volume, as temporary folders are never one.
    fn any_folder(path: &Path) -> Result<PathBuf, Refusal> {
        std::fs::canonicalize(path).map_err(|_| Refusal::NotAVolume)
    }

    /// A home folder with notes, a volume with a photo, and a core scanning the home folder.
    fn home_and_volume(
        full_disk: u32,
        events: &Events,
    ) -> (Core, [tempfile::TempDir; 2], [PathBuf; 2]) {
        let (home_dir, home) = home_with(&["notes/a.txt"]);
        let (volume_dir, volume) = home_with(&["photos/beach.jpg"]);
        let core = scan(home.to_str().unwrap(), full_disk, events);
        events.wait_for_event(TL_EVENT_INDEXED);
        unsafe { (*core.0).judge = Some(any_folder) };
        (core, [home_dir, volume_dir], [home, volume])
    }

    fn mounted(core: &Core, volume: &Path, mark: u64) -> u32 {
        let path = volume.to_str().unwrap();
        unsafe { tl_volume_added(core.0, path.as_ptr(), path.len(), mark) }
    }

    fn unmounted(core: &Core, volume: &Path, mark: u64) -> u32 {
        let path = volume.to_str().unwrap();
        unsafe { tl_volume_removed(core.0, path.as_ptr(), path.len(), mark) }
    }

    #[test]
    fn a_volume_joins_and_leaves_without_the_home_folder_read_again() {
        let events = Events::default();
        let (core, _dirs, [home, volume]) = home_and_volume(1, &events);
        // Never told, so only a full read of the home folder would find it.
        std::fs::write(home.join("notes/unseen.txt"), "").unwrap();

        assert_eq!(mounted(&core, &volume, 5), 1);
        assert_eq!(mounted(&core, &volume, 6), 0, "already added");
        // The home folder, notes and a.txt, then the volume, photos and the beach.
        assert_eq!(events.wait_for_rebuilt(1), 6);
        assert_eq!(found(&core, &events, "beach"), ["beach.jpg"]);
        assert!(found(&core, &events, "unseen").is_empty());
        assert_eq!(watched(&core).1, 5);
        let volume_text = volume.to_string_lossy().into_owned();
        assert!(watched_inside(&core).contains(&volume_text));

        // A change on the volume reads only the folder it is in.
        std::fs::write(volume.join("photos/dunes.jpg"), "").unwrap();
        assert_eq!(changed_at(&core, &[&volume.join("photos")], 0, 6), 1);
        assert_eq!(events.wait_for_rebuilt(2), 7);
        assert_eq!(found(&core, &events, "dunes"), ["dunes.jpg"]);
        assert!(found(&core, &events, "unseen").is_empty());
        assert_eq!(watched(&core).1, 6);

        assert_eq!(unmounted(&core, &volume, 7), 1);
        assert!(
            !watched_inside(&core).contains(&volume_text),
            "let go of before the list without it is built"
        );
        assert_eq!(unmounted(&core, &volume, 8), 0, "already gone");
        assert_eq!(events.wait_for_rebuilt(3), 3);
        assert!(found(&core, &events, "beach").is_empty());
        assert!(!watched_inside(&core).contains(&volume_text));
        assert_eq!(watched(&core).1, 7);

        assert_eq!(mounted(&core, &volume, 9), 1);
        assert_eq!(events.wait_for_rebuilt(4), 7);
        // A change in the home folder still brings a whole new list, the volume read again.
        assert_eq!(changed_at(&core, &[&home.join("notes")], 0, 10), 1);
        assert_eq!(events.wait_for_rebuilt(5), 8);
        assert_eq!(found(&core, &events, "unseen"), ["unseen.txt"]);
        assert_eq!(found(&core, &events, "dunes"), ["dunes.jpg"]);
    }

    #[test]
    fn a_volume_taken_out_mid_scan_is_let_go_of_before_the_call_returns() {
        let events = Events::default();
        let (core, _dirs, [_, _]) = home_and_volume(1, &events);
        let folders: Vec<String> = (0..20_000).map(|n| format!("f{}/g{n}/", n % 100)).collect();
        let folders: Vec<&str> = folders.iter().map(String::as_str).collect();
        let (_big_dir, big) = home_with(&folders);
        let live = unsafe { &(*core.0).live };
        assert_eq!(mounted(&core, &big, 5), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.reading.try_lock().is_ok() {
            assert!(
                Instant::now() < deadline,
                "never caught the volume being read"
            );
        }
        assert_eq!(unmounted(&core, &big, 6), 1);
        assert!(
            live.reading.try_lock().is_ok(),
            "the scan still reads the volume"
        );
        let big_text = big.to_string_lossy().into_owned();
        assert!(!watched_inside(&core).contains(&big_text));
    }

    #[test]
    fn a_pass_planned_before_an_eject_never_reads_the_drive() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_v, volume) = home_with(&["photos/beach.jpg"]);
        let live = live(Duration::ZERO);
        live.plan(|plan| plan.volumes.push(volume.clone()));
        // The pass copies the drive, then reads the home folder while the drive is ejected.
        let volumes = live.plan.lock().unwrap().volumes.clone();
        live.plan(|plan| assert!(plan.forget(&volume)));
        live.ejected.fetch_add(1, Ordering::Relaxed);

        let events = Events::default();
        let notify = Notify {
            ctx: &events as *const Events as *mut c_void,
            on_event: record,
        };
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let mut index = settings.scan().unwrap().index;
        assert!(!live.add_volumes(&settings, &mut index, &volumes, &notify));
        assert_eq!(index.volumes().count(), 0);
    }

    #[test]
    fn a_volume_waits_for_full_disk_access() {
        let events = Events::default();
        let (core, _dirs, [_, volume]) = home_and_volume(0, &events);
        assert_eq!(mounted(&core, &volume, 2), 0);
        unsafe { tl_grant_full_disk(core.0, 0, 3) };
        assert_eq!(events.wait_for_rebuilt(1), 6);
        assert_eq!(found(&core, &events, "beach"), ["beach.jpg"]);
    }

    #[test]
    fn a_volume_taken_out_before_access_never_joins() {
        let events = Events::default();
        let (core, _dirs, [_, volume]) = home_and_volume(0, &events);
        assert_eq!(mounted(&core, &volume, 2), 0);
        assert_eq!(unmounted(&core, &volume, 3), 0);
        unsafe { tl_grant_full_disk(core.0, 0, 4) };
        assert_eq!(events.wait_for_rebuilt(1), 3);
    }

    #[test]
    fn a_core_refuses_volumes_it_cannot_take() {
        let events = Events::default();
        let (core, [_home, volume_dir], [_, volume]) = home_and_volume(1, &events);
        let refuse = |path: *const u8, len| unsafe { tl_volume_added(core.0, path, len, 1) };
        assert_eq!(refuse(std::ptr::null(), 0), 0);
        assert_eq!(refuse([0xff].as_ptr(), 1), 0);
        let gone = volume_dir.path().join("gone");
        assert_eq!(mounted(&core, &gone, 1), 0, "refused by the judge");
        assert_eq!(unmounted(&core, &volume, 1), 0, "never added");
        let removed = unsafe { tl_volume_removed(core.0, std::ptr::null(), 0, 1) };
        assert_eq!(removed, 0);

        // Made-up names have no volumes.
        let stand_in_events = Events::default();
        let stand_in = start(100, &stand_in_events);
        assert_eq!(mounted(&stand_in, &volume, 1), 0);

        // Nor does a core that failed.
        let failed_events = Events::default();
        let failed = scan(
            volume_dir.path().join("gone").to_str().unwrap(),
            1,
            &failed_events,
        );
        failed_events.wait_for_event(TL_EVENT_FAILED);
        unsafe { (*failed.0).judge = Some(any_folder) };
        assert_eq!(mounted(&failed, &volume, 1), 0);
        // Nothing waits, so no new list is coming.
        for core in [&core, &failed] {
            let live = unsafe { &(*core.0).live };
            let plan = live.plan.lock().unwrap();
            assert!(plan.volumes.is_empty() && !plan.patching && plan.patched.is_none());
        }
    }

    /// A home folder's list published as the newest, and the source that built it, so a
    /// test drives `graft` itself.
    fn home_source(home: &Path, live: &Live, notify: &Notify) -> Source {
        let mut source = Source::Home {
            root: home.to_path_buf(),
            access: Access::Full,
            told: Vec::new(),
            settings: None,
            journal: None,
            unreadable: 0,
        };
        let built = source.build(Clutter::Skip, &[], live, notify).unwrap();
        live.publish(built, 0);
        live.plan(|plan| plan.granted = true);
        source
    }

    fn notify_to(events: &Events) -> Notify {
        let ctx = events as *const Events as *mut c_void;
        Notify {
            ctx,
            on_event: record,
        }
    }

    /// Grafts what the plan holds onto the newest list, which then becomes the newest.
    fn graft(source: &Source, live: &Live, notify: &Notify) -> Option<u64> {
        let (volumes, changes) = live.plan.lock().unwrap().volumes();
        let (list, scope) = source.graft(&volumes, &changes, live, notify)?;
        let list = live.publish_list(list, scope, 0);
        Some(read(&list).unwrap().index.entry_count() as u64)
    }

    #[test]
    fn a_volume_plugged_in_and_out_within_one_gap_brings_no_list() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_v, volume) = home_with(&["photos/beach.jpg"]);
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let source = home_source(&home, &live, &notify);
        live.plan(|plan| {
            plan.volumes.push(volume.clone());
            plan.forget(&volume);
        });
        assert_eq!(graft(&source, &live, &notify), None);
    }

    #[test]
    fn a_list_read_before_an_eject_leaves_the_drive_unwatched() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_a, photos) = home_with(&["beach.jpg"]);
        let (_b, music) = home_with(&["song.mp3"]);
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let source = home_source(&home, &live, &notify);
        live.plan(|plan| plan.volumes = vec![photos.clone(), music.clone()]);
        let (volumes, changes) = live.plan.lock().unwrap().volumes();
        let (list, scope) = source.graft(&volumes, &changes, &live, &notify).unwrap();
        live.plan(|plan| assert!(plan.forget(&photos)));
        live.publish_list(list, scope, 0);
        let watched: Vec<String> = live
            .watched
            .lock()
            .unwrap()
            .0
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert!(!watched.contains(&photos.to_string_lossy().into_owned()));
        assert!(
            watched.contains(&music.to_string_lossy().into_owned()),
            "still mounted"
        );
        assert!(watched.contains(&home.to_string_lossy().into_owned()));
    }

    #[test]
    fn two_volumes_plugged_in_within_one_gap_bring_one_list() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_a, photos) = home_with(&["beach.jpg"]);
        let (_b, music) = home_with(&["song.mp3"]);
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let source = home_source(&home, &live, &notify);
        live.plan(|plan| plan.volumes = vec![photos.clone(), music.clone()]);
        assert_eq!(graft(&source, &live, &notify), Some(7));
        let newest = live.newest.lock().unwrap().clone().unwrap().1;
        let listed = read(&newest).unwrap();
        let held: Vec<&Path> = listed.index.volumes().collect();
        assert_eq!(held, [photos.as_path(), music.as_path()]);
        drop(listed);
        assert_eq!(graft(&source, &live, &notify), None, "already held");
    }

    #[test]
    fn a_volume_that_cannot_be_read_is_forgotten_and_told() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_v, volume) = home_with(&["photos/beach.jpg"]);
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let source = home_source(&home, &live, &notify);
        let unplugged = volume.with_extension("unplugged");
        std::fs::rename(&volume, &unplugged).unwrap();
        live.plan(|plan| plan.volumes.push(volume.clone()));
        assert_eq!(graft(&source, &live, &notify), None);
        assert!(live.plan.lock().unwrap().volumes.is_empty());
        let notes = events.notes.lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with(&format!("{} could not be read", volume.display())));

        // Told again once it is back, it joins.
        std::fs::rename(&unplugged, &volume).unwrap();
        live.plan(|plan| plan.volumes.push(volume.clone()));
        assert_eq!(graft(&source, &live, &notify), Some(6));
    }

    #[test]
    fn a_changed_folder_is_read_again_alone() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_v, volume) = home_with(&["photos/beach.jpg"]);
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let source = home_source(&home, &live, &notify);
        live.plan(|plan| plan.volumes.push(volume.clone()));
        assert_eq!(graft(&source, &live, &notify), Some(6));

        std::fs::write(home.join("notes/unseen.txt"), "").unwrap();
        std::fs::write(volume.join("photos/dunes.jpg"), "").unwrap();
        let (photos, notes) = (volume.join("photos"), home.join("notes"));
        let on_volume = Missed::Folders(vec![photos.clone()]);
        assert_eq!(live.misses(&[volume.clone(), photos.clone()]), on_volume);
        let both = Missed::Folders(vec![photos.clone(), notes.clone()]);
        assert_eq!(live.misses(&[photos.clone(), notes.clone()]), both);
        live.plan(|plan| plan.reread(vec![photos.clone()], 3));
        assert_eq!(graft(&source, &live, &notify), Some(7));
        assert_eq!(
            live.misses(&[volume.clone(), photos.clone()]),
            Missed::Nothing
        );
        let on_home = Missed::Folders(vec![notes.clone()]);
        assert_eq!(live.misses(std::slice::from_ref(&notes)), on_home);
        live.plan(|plan| plan.reread(vec![notes.clone()], 4));
        assert_eq!(graft(&source, &live, &notify), Some(8));
        let all = [home.clone(), notes.clone(), volume.clone(), photos.clone()];
        assert_eq!(live.misses(&all), Missed::Nothing);
        // Your ignore list's folder, even unchanged: only a scan by the new rules is sure.
        let list = home.join(".config/argus");
        assert_eq!(live.misses(&[notes.clone(), list]), Missed::More);

        // Asked for a whole new list already, which reads them anyway.
        live.plan(|plan| {
            plan.want(4);
            plan.reread(vec![photos.clone()], 5);
            assert!(plan.changes.is_empty());
            assert_eq!(plan.asked, Some(4));
        });
    }

    #[test]
    fn a_volume_added_just_as_access_is_granted_is_never_left_out() {
        let (_a, photos) = home_with(&["beach.jpg"]);
        let (_b, music) = home_with(&["song.mp3"]);
        let live = Arc::new(Live::default());
        let core = idle_core(&live);
        unsafe { (*core.0).judge = Some(any_folder) };
        // Told first: remembered, and the list access brings reads it.
        assert_eq!(mounted(&core, &photos, 1), 0);
        live.plan(|plan| assert_eq!(plan.volumes(), (Vec::new(), Vec::new())));
        unsafe { tl_grant_full_disk(core.0, 0, 2) };
        live.plan(|plan| {
            assert!(plan.wanted && plan.granted);
            assert_eq!(plan.volumes().0, std::slice::from_ref(&photos));
        });
        // Granted first: read at once.
        assert_eq!(mounted(&core, &music, 3), 1);
        live.plan(|plan| {
            assert!(plan.patching);
            assert_eq!(plan.patched, Some(3));
            assert_eq!(plan.volumes().0, [photos.clone(), music.clone()]);
        });
    }

    #[test]
    fn access_granted_before_the_first_list_still_counts() {
        let (_dir, home) = home_with(&["Documents/budget.pdf"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 0, &events);
        unsafe { tl_grant_full_disk(core.0, 0, 1) };
        events.wait_for_event(TL_EVENT_INDEXED);
        assert_eq!(events.wait_for_rebuilt(1), 3);
    }

    #[test]
    fn a_file_saved_over_needs_no_rebuild() {
        let (_dir, home) = home_with(&["notes/budget.txt", "notes/node_modules/", ".claude.json"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 4);
        std::fs::write(home.join("notes/budget.txt"), "edited").unwrap();
        std::fs::write(home.join(".claude.json.tmp"), "{}").unwrap();
        std::fs::rename(home.join(".claude.json.tmp"), home.join(".claude.json")).unwrap();
        assert_eq!(changed(&core, &[&home, &home.join("notes")], 0), 0);

        // While a rebuild runs, the list on hand may be older than the disk, so any folder
        // the list reads counts, and still only those.
        unsafe { &(*core.0).live }.plan(|plan| plan.running = true);
        let clutter = [home.join("notes/node_modules"), home.join("Library/Caches")];
        for folder in &clutter {
            assert_eq!(changed(&core, &[folder], 0), 0, "{folder:?}");
        }
        assert_eq!(changed(&core, &[&home.join("notes")], 0), 1);
    }

    #[test]
    fn a_failed_rebuild_keeps_the_list_in_use() {
        let (dir, home) = home_with(&["notes/budget.txt"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        drop(dir);
        assert_eq!(changed(&core, &[], 1), 1);
        let asked = Instant::now();
        while events.notes.lock().unwrap().is_empty() && asked.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let notes = events.notes.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("could not be built again"),
            "{}",
            notes[0]
        );
        drop(notes);

        let ticket = submit(&core, "budget");
        events.wait_for(TL_EVENT_READY, ticket);
        assert_eq!(take(&core, ticket).unwrap().len(), 1);
        assert!(events.rebuilt().is_empty());
    }

    /// A bug that stops a patch halfway leaves a list no search may read: searches find
    /// nothing, and the core goes on, until a whole new list takes its place.
    #[test]
    fn a_list_a_bug_broke_mid_patch_finds_nothing_until_read_whole() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let live = unsafe { &(*core.0).live };
        let list = live.newest.lock().unwrap().clone().unwrap().1;
        let _ = thread::spawn(move || {
            let _patching = list.write().unwrap();
            panic!("a bug in the engine");
        })
        .join();
        assert!(found(&core, &events, "budget").is_empty());
        assert!(!holds(&core, &home.join("notes")));
        assert_eq!(changed(&core, &[], 1), 1);
        assert_eq!(events.wait_for_rebuilt(1), 3);
        assert_eq!(found(&core, &events, "budget"), ["budget.txt"]);
        assert!(holds(&core, &home.join("notes")));
        assert_eq!(events.count(TL_EVENT_FAILED, 0), 0);
    }

    /// The window asks on its main thread, so the answer never waits for a patch: meanwhile,
    /// a folder the scan reads counts as held.
    #[test]
    fn whether_a_folder_is_held_is_told_at_once_during_a_patch() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let live = unsafe { &(*core.0).live };
        let list = live.newest.lock().unwrap().clone().unwrap().1;
        let _patching = list.write().unwrap();
        assert!(holds(&core, &home.join("notes")));
        assert!(!holds(&core, Path::new("/")));
    }

    #[test]
    fn freeing_the_core_cuts_a_waiting_rebuild_short() {
        // Kept for good, as the core may outlive a failed check.
        let events: &'static Events = Box::leak(Box::default());
        let ctx = events as *const Events as *mut c_void;
        let (source, gap) = (Source::StandIn(100), Duration::from_secs(3600));
        let live = Live {
            gap,
            ..Live::default()
        };
        let core = Core(super::start(ctx, Some(record), source, false, None, live));
        events.wait_for(TL_EVENT_INDEXED, 100);
        // This one waits out an hour's gap after the first list.
        assert_eq!(changed(&core, &[], 1), 1);
        let core = std::mem::ManuallyDrop::new(core).0 as usize;
        let (done, freed) = mpsc::channel();
        thread::spawn(move || {
            unsafe { tl_core_free(core as *mut TlCore) };
            let _ = done.send(());
        });
        let freed = freed.recv_timeout(Duration::from_secs(60));
        assert!(freed.is_ok(), "freeing waited out the gap");
    }

    /// A core over `home` that saves its list to `file` under `journal` 20 ms after each new
    /// list, started when the newest FSEvents event id was `now`.
    fn saving_core(home: &Path, file: &Path, journal: &[u8], now: u64, events: &Events) -> Core {
        let live = Live {
            save_after: Duration::from_millis(20),
            ..Live::default()
        };
        saving_core_over(live, home, file, journal, now, events)
    }

    fn saving_core_over(
        live: Live,
        home: &Path,
        file: &Path,
        journal: &[u8],
        now: u64,
        events: &Events,
    ) -> Core {
        let ctx = events as *const Events as *mut c_void;
        let source = Source::Home {
            root: home.to_path_buf(),
            access: Access::Full,
            told: Vec::new(),
            settings: None,
            journal: Some(journal.to_vec()),
            unreadable: 0,
        };
        let resume = Some((file.to_path_buf(), now));
        let core = super::start(ctx, Some(record), source, false, resume, live);
        assert!(!core.is_null());
        Core(core)
    }

    /// The FSEvents event id in a saved list's header, or `None` before there is one.
    fn saved_event(file: &Path) -> Option<u64> {
        let bytes = std::fs::read(file).ok()?;
        let id = bytes
            .get(16..24)
            .filter(|_| bytes.starts_with(b"TL-INDEX"))?;
        Some(u64::from_le_bytes(id.try_into().unwrap()))
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(Instant::now() < until, "{what} never happened");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_saved_list_opens_at_once_and_catches_up_from_its_event() {
        let (_dir, home) = home_with(&["notes/budget.txt", "work/plan.md"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("io.github.jovanhartono.argus/index");
        let notes = home.join("notes");
        {
            let events = Events::default();
            let _core = saving_core(&home, &file, b"5F2A", 50, &events);
            events.wait_for(TL_EVENT_INDEXED, 5);
            // Current to the event the scan started after; its folder was made for it.
            wait_until("the first save", || saved_event(&file) == Some(50));
            assert!(events.notes.lock().unwrap().is_empty());
        }

        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 60, &events);
        events.wait_for(TL_EVENT_OPENED, 5);
        assert_eq!(events.count(TL_EVENT_INDEXED, 5), 0);
        let opened = health(&core);
        assert_eq!(
            (opened.entries, opened.scanned_at, opened.changed_at),
            (5, 0, 0)
        );
        assert_eq!(
            watched(&core).1,
            50,
            "the window replays from the saved event"
        );
        assert_eq!(found(&core, &events, "budget"), ["budget.txt"]);

        // A change made while it was closed, replayed: read again in place, saved at its event.
        std::fs::write(notes.join("todo.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 55), 1);
        assert_eq!(events.wait_for_rebuilt(1), 6);
        let patched = health(&core);
        assert_eq!(
            (patched.entries, patched.scanned_at),
            (6, 0),
            "still the saved list"
        );
        assert_ne!(patched.changed_at, 0);
        wait_until("the save after a change", || saved_event(&file) == Some(55));
        // Saved over, so nothing to read again, and no new list to save within the hour.
        std::fs::write(notes.join("todo.md"), "edited").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 58), 0);
        assert!(
            unsafe { &(*core.0).live }
                .saving
                .lock()
                .unwrap()
                .waiting
                .is_none()
        );
        drop(core);
        assert_eq!(saved_event(&file), Some(55));
        assert!(events.notes.lock().unwrap().is_empty());

        // Saved at an event the log has since lost: shown at once, then built again.
        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 40, &events);
        events.wait_for(TL_EVENT_OPENED, 6);
        assert_eq!(watched(&core).1, 0);
        assert_eq!(events.wait_for_rebuilt(1), 6);
        assert_ne!(health(&core).scanned_at, 0, "read from the folder again");
        wait_until("the save after a new list", || {
            saved_event(&file) == Some(40)
        });
        drop(core);

        // Another log: its event ids mean nothing here, so the folder is scanned.
        let events = Events::default();
        let core = saving_core(&home, &file, b"77C0", 90, &events);
        events.wait_for(TL_EVENT_INDEXED, 6);
        assert_eq!(events.count(TL_EVENT_OPENED, 6), 0);
        assert_eq!(watched(&core).1, 0);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_list_saved_without_an_event_is_built_again_after_opening() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        {
            let events = Events::default();
            let _core = saving_core(&home, &file, b"", 0, &events);
            events.wait_for(TL_EVENT_INDEXED, 3);
            wait_until("the first save", || saved_event(&file) == Some(0));
        }
        std::fs::write(home.join("notes/plan.md"), "").unwrap();
        let events = Events::default();
        let _core = saving_core(&home, &file, b"", 70, &events);
        events.wait_for(TL_EVENT_OPENED, 3);
        assert_eq!(events.wait_for_rebuilt(1), 4);
        wait_until("the save after a new list", || {
            saved_event(&file) == Some(70)
        });
    }

    #[test]
    fn a_damaged_saved_list_is_told_and_scanned_again() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        std::fs::write(&file, "not a list").unwrap();
        let events = Events::default();
        let _core = saving_core(&home, &file, b"5F2A", 9, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let notes = events.wait_for_note("saved list could not be opened");
        assert_eq!(notes.len(), 1, "{notes:?}");
        wait_until("the good list replacing it", || {
            saved_event(&file) == Some(9)
        });
    }

    #[test]
    fn a_list_that_cannot_be_saved_is_told_and_search_goes_on() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        // Its folder would have to be made inside a file.
        let file = home.join("notes/budget.txt/index");
        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 9, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        let notes = events.wait_for_note("the list could not be saved");
        // Windows reads a path through a file as missing, so only Unix notes the failed open.
        if cfg!(windows) {
            assert_eq!(notes.len(), 1, "{notes:?}");
        } else {
            assert_eq!(notes.len(), 2, "{notes:?}");
            assert!(notes[0].starts_with("the saved list could not be opened"));
        }
        assert_eq!(found(&core, &events, "budget"), ["budget.txt"]);
    }

    #[test]
    fn a_core_given_a_saved_list_through_the_hatch_opens_it() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        {
            let events = Events::default();
            let _core = saving_core(&home, &file, b"5F2A", 50, &events);
            wait_until("the first save", || saved_event(&file) == Some(50));
        }
        let path = file.to_str().unwrap().as_bytes();
        let events = Events::default();
        let saved = saved_at(path, b"5F2A", 60);
        let core = scan_saving(home.to_str().unwrap(), 1, 0, &saved, &events);
        events.wait_for(TL_EVENT_OPENED, 3);
        assert_eq!(watched(&core).1, 50);
        drop(core);

        // Without Full Disk Access a scan reads other folders, so the list is scanned.
        let events = Events::default();
        let _core = scan_saving(home.to_str().unwrap(), 0, 0, &saved, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        assert_eq!(events.count(TL_EVENT_OPENED, 3), 0);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_list_saved_at_the_newest_event_opens_with_nothing_to_build() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        {
            let events = Events::default();
            let _core = saving_core(&home, &file, b"5F2A", 50, &events);
            wait_until("the first save", || saved_event(&file) == Some(50));
        }
        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 50, &events);
        events.wait_for(TL_EVENT_OPENED, 3);
        assert_eq!(watched(&core).1, 50);
        thread::sleep(2 * GAP);
        assert!(events.rebuilt().is_empty());
        assert!(
            unsafe { &(*core.0).live }
                .saving
                .lock()
                .unwrap()
                .waiting
                .is_none()
        );
    }

    #[test]
    fn a_list_an_older_release_saved_is_scanned_again_without_a_note() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        {
            let events = Events::default();
            let _core = saving_core(&home, &file, b"5F2A", 50, &events);
            wait_until("the first save", || saved_event(&file) == Some(50));
        }
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&file, bytes).unwrap();
        let events = Events::default();
        let _core = saving_core(&home, &file, b"5F2A", 60, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        wait_until("the save in its place", || saved_event(&file) == Some(60));
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_rebuild_that_failed_is_tried_again_whole_at_the_next_change() {
        let (dir, home) = home_with(&["notes/budget.txt", "work/plan.md"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 50, &events);
        events.wait_for(TL_EVENT_INDEXED, 5);
        wait_until("the first save", || saved_event(&file) == Some(50));

        // macOS lost track of a change while the home folder couldn't be read.
        let away = dir.path().with_extension("away");
        std::fs::rename(&home, &away).unwrap();
        std::fs::write(away.join("work/new.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[], 1, 60), 1);
        events.wait_for_note("could not be built again");
        std::fs::rename(&away, &home).unwrap();
        let live = unsafe { &(*core.0).live };
        wait_until("the rebuild to end", || !live.plan.lock().unwrap().running);
        assert_eq!(live.plan.lock().unwrap().failed, Some(60));
        let behind = health(&core);
        assert!(
            behind.behind && !behind.busy && !behind.stopped,
            "{behind:?}"
        );
        assert_eq!(behind.entries, 5, "the list in use is kept");

        // A change elsewhere brings the whole list, with the change it missed.
        let notes = home.join("notes");
        std::fs::write(notes.join("todo.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 70), 1);
        assert_eq!(events.wait_for_rebuilt(1), 7);
        assert_eq!(found(&core, &events, "new"), ["new.md"]);
        wait_until("the save", || saved_event(&file) == Some(70));
        assert_eq!(live.plan.lock().unwrap().failed, None);
        assert!(!health(&core).behind);
    }

    #[test]
    fn a_folder_read_again_after_a_failed_rebuild_brings_a_whole_one() {
        let mut plan = Plan {
            failed: Some(4),
            ..Plan::default()
        };
        plan.reread(vec!["/home/notes".into()], 6);
        assert!(plan.wanted && !plan.patching);
        assert_eq!(plan.asked, Some(4));
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn the_saver_clears_what_a_quit_mid_save_left() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let left = support.path().join("index.99999.tmp");
        let writing = support.path().join("index.99998.tmp");
        std::fs::File::create(&left)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - 2 * LEFT_FOR)
            .unwrap();
        std::fs::write(&writing, "").unwrap();
        let events = Events::default();
        let _core = saving_core(&home, &file, b"5F2A", 50, &events);
        wait_until("the first save", || saved_event(&file) == Some(50));
        assert!(!left.exists());
        assert!(writing.exists(), "another copy of the app may be saving");
    }

    #[test]
    fn edits_that_change_no_names_move_the_saved_event_on() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let notes = home.join("notes");
        let live = Live {
            save_after: Duration::from_millis(20),
            resave: Duration::ZERO,
            ..Live::default()
        };
        let events = Events::default();
        let core = saving_core_over(live, &home, &file, b"5F2A", 50, &events);
        events.wait_for(TL_EVENT_INDEXED, 3);
        wait_until("the first save", || saved_event(&file) == Some(50));
        std::fs::write(notes.join("budget.txt"), "edited").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 58), 0);
        wait_until("the save at the edit", || saved_event(&file) == Some(58));
        // A mark FSEvents didn't give moves nothing on.
        assert_eq!(changed_at(&core, &[&notes], 0, 0), 0);
        assert!(
            unsafe { &(*core.0).live }
                .saving
                .lock()
                .unwrap()
                .waiting
                .is_none()
        );
        assert!(events.rebuilt().is_empty());
    }

    #[test]
    fn the_saver_writing_its_own_folder_asks_for_no_new_list() {
        let (_dir, home) = home_with(&["notes/budget.txt", "support/readme.txt"]);
        let support = home.join("support");
        let file = support.join("index");
        let events = Events::default();
        let core = saving_core(&home, &file, b"5F2A", 50, &events);
        events.wait_for(TL_EVENT_INDEXED, 5);
        wait_until("the first save", || saved_event(&file) == Some(50));
        assert_eq!(changed_at(&core, &[&support], 0, 60), 0);

        let notes = home.join("notes");
        std::fs::write(notes.join("todo.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&support, &notes], 0, 61), 1);
        assert_eq!(events.wait_for_rebuilt(1), 6);
        assert_eq!(found(&core, &events, "todo"), ["todo.md"]);
        assert_eq!(found(&core, &events, "readme"), ["readme.txt"]);
    }

    #[test]
    fn a_saved_list_leaves_its_volumes_out() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (_v, volume) = home_with(&["photos/beach.jpg"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let mut source = Source::Home {
            root: home.clone(),
            access: Access::Full,
            told: Vec::new(),
            settings: None,
            journal: Some(b"5F2A".to_vec()),
            unreadable: 0,
        };
        let mut index = source
            .build(Clutter::Skip, &[], &live, &notify)
            .unwrap()
            .index;
        let (settings, _) = source.saving().unwrap();
        settings.add_volume(&mut index, &volume).unwrap();
        assert_eq!(index.entry_count(), 6);
        save_index(&file, &index, 2, &settings, 9).unwrap();

        let (built, saved_at) = source.open(&file, Clutter::Skip, &notify).unwrap();
        assert_eq!(saved_at, 9);
        assert_eq!(built.index.volumes().count(), 0);
        assert_eq!(built.index.entry_count(), 3);
        assert_eq!(source.saving().unwrap().1, 2);
        // Kept for a list built with other rules.
        assert!(source.open(&file, Clutter::Keep, &notify).is_none());
        assert!(events.notes.lock().unwrap().is_empty());
    }

    /// A saver over `live` for the length of `body`, stopped after it.
    fn with_saver(live: &Live, events: &Events, body: impl FnOnce()) {
        let notify = notify_to(events);
        thread::scope(|scope| {
            let saver = scope.spawn(|| save_loop(live, notify));
            body();
            live.saving.lock().unwrap().stopped = true;
            live.save.notify_all();
            assert!(saver.join().is_ok());
        });
    }

    fn saving_live(file: &Path, after: Duration, within: Duration) -> Live {
        Live {
            save_after: after,
            save_within: within,
            saving: Mutex::new(Saving {
                file: Some(file.to_path_buf()),
                ..Saving::default()
            }),
            ..Live::default()
        }
    }

    #[test]
    fn a_burst_of_new_lists_is_saved_once_as_the_last() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let hour = Duration::from_secs(3600);
        let live = saving_live(&file, Duration::from_secs(1), hour);
        let events = Events::default();
        let source = home_source(&home, &live, &notify_to(&events));
        let index = live.newest.lock().unwrap().clone().unwrap().1;
        // Kept before the saver starts, so only one list can be written.
        for event_id in 1..=3 {
            live.keep(&index, &source, event_id);
            thread::sleep(Duration::from_millis(20));
        }
        {
            let saving = live.saving.lock().unwrap();
            let waiting = saving.waiting.as_ref().unwrap();
            assert_eq!(waiting.event_id, 3);
            // The cap counts from the first.
            assert!(waiting.last - waiting.first >= Duration::from_millis(40));
        }
        with_saver(&live, &events, || {
            wait_until("the save", || saved_event(&file) == Some(3));
            assert!(live.saving.lock().unwrap().waiting.is_none());
        });
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn files_that_never_stop_changing_still_get_saved() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let hour = Duration::from_secs(3600);
        let live = saving_live(&file, hour, Duration::from_millis(50));
        let events = Events::default();
        let source = home_source(&home, &live, &notify_to(&events));
        let index = live.newest.lock().unwrap().clone().unwrap().1;
        with_saver(&live, &events, || {
            let mut event_id = 0;
            while saved_event(&file).is_none() {
                assert!(event_id < 6_000, "never saved while lists kept coming");
                event_id += 1;
                live.keep(&index, &source, event_id);
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_list_still_waiting_when_the_core_stops_goes_unsaved() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let hour = Duration::from_secs(3600);
        let live = saving_live(&file, hour, hour);
        let events = Events::default();
        let source = home_source(&home, &live, &notify_to(&events));
        let index = live.newest.lock().unwrap().clone().unwrap().1;
        // Stopped without waiting the hour.
        with_saver(&live, &events, || live.keep(&index, &source, 1));
        assert!(!file.exists());
    }

    #[test]
    fn a_change_that_needs_nothing_moves_on_the_list_to_save() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let hour = Duration::from_secs(3600);
        let events = Events::default();
        let waiting = |live: &Live| {
            let saving = live.saving.lock().unwrap();
            saving.waiting.as_ref().map(|waiting| waiting.event_id)
        };

        let live = saving_live(&file, hour, hour);
        let source = home_source(&home, &live, &notify_to(&events));
        let index = live.newest.lock().unwrap().clone().unwrap().1;
        live.caught_up(3);
        assert_eq!(waiting(&live), None, "nothing kept yet");
        live.keep(&index, &source, 5);
        live.caught_up(9);
        live.caught_up(7);
        assert_eq!(waiting(&live), Some(9));
        // Saved since, and kept too lately to save again.
        live.saving.lock().unwrap().waiting = None;
        live.caught_up(12);
        assert_eq!(waiting(&live), None);
        // Opened at launch, so it may be saved again at once.
        live.opened(&source, 5);
        live.caught_up(5);
        assert_eq!(waiting(&live), None, "already current to it");
        live.caught_up(13);
        assert_eq!(waiting(&live), Some(13));

        let live = Live {
            resave: Duration::ZERO,
            ..saving_live(&file, hour, hour)
        };
        let source = home_source(&home, &live, &notify_to(&events));
        live.keep(&index, &source, 5);
        live.saving.lock().unwrap().waiting = None;
        live.caught_up(12);
        assert_eq!(waiting(&live), Some(12));
    }

    #[test]
    fn a_core_without_a_file_saves_nothing() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let (live, events) = (Live::default(), Events::default());
        let source = home_source(&home, &live, &notify_to(&events));
        let index = live.newest.lock().unwrap().clone().unwrap().1;
        live.keep(&index, &source, 1);
        assert!(live.saving.lock().unwrap().waiting.is_none());
        // Nor are made-up names saved.
        let saving = saving_live(&home.join("index"), Duration::ZERO, Duration::ZERO);
        saving.keep(&index, &Source::StandIn(10), 1);
        assert!(saving.saving.lock().unwrap().waiting.is_none());
    }

    /// A pass reading `changes` again, as `rebuild_loop` hands one on.
    fn patch(changes: Vec<PathBuf>, event_id: u64) -> Pass {
        Pass {
            full_disk: false,
            clutter: None,
            mark: event_id,
            current: event_id,
            volumes: Vec::new(),
            changes,
        }
    }

    /// A patch waits out a save under way, which reads the list for as long as the disk takes,
    /// and searches go on meanwhile.
    #[test]
    fn a_save_under_way_holds_back_a_patch_but_never_a_search() {
        let (_h, home) = home_with(&["notes/a.txt"]);
        let notes = home.join("notes");
        let (live, events) = (Live::default(), Events::default());
        let notify = notify_to(&events);
        let mut source = home_source(&home, &live, &notify);
        let list = live.newest.lock().unwrap().clone().unwrap().1;
        let (searcher, done, opens) =
            (Searcher::with_threads(1), Done::default(), Mutex::default());
        let (jobs, inbox) = mpsc::channel();
        let (swaps, swapped) = mpsc::channel();
        std::fs::write(notes.join("budget.txt"), "").unwrap();
        thread::scope(|threads| {
            let (worker_list, searcher, done, opens) =
                (Arc::clone(&list), &searcher, &done, &opens);
            threads
                .spawn(move || serve(worker_list, searcher, &inbox, done, opens, &notify, RERANK));
            let search = |query: &str| {
                let ticket = searcher.ticket();
                let (query, folder) = (query.to_owned(), None);
                jobs.send(Job::Search {
                    ticket,
                    query,
                    folder,
                })
                .unwrap();
                ticket.number()
            };
            // As `save_loop` holds them while it writes the list.
            let writing = lock(&live.writing);
            let saving = read(&list).unwrap();
            let patching = threads.spawn(|| {
                let pass = patch(vec![notes.clone()], 1);
                rebuild(&mut source, pass, &live, &swaps, &notify);
            });
            // Time for the patch to reach the save and wait.
            thread::sleep(Duration::from_millis(100));
            let ticket = search("a");
            assert!(events.heard(TL_EVENT_READY, ticket), "a search waited");
            assert!(!patching.is_finished() && swapped.try_recv().is_err());
            drop((saving, writing));
            patching.join().unwrap();
            assert!(matches!(swapped.try_recv(), Ok(Job::Swap { .. })));
            assert_eq!(read(&list).unwrap().version, 1);
            let ticket = search("budget");
            events.wait_for(TL_EVENT_READY, ticket);
            let (_, found) = done.lock().unwrap().take().unwrap();
            assert_eq!(paths_of(&found), [notes.join("budget.txt")]);
            drop(jobs);
        });
    }

    /// Every entry's path and kind, sorted, so lists numbered apart compare.
    fn listing(index: &Index) -> Vec<(PathBuf, bool)> {
        let kept = index.ids().filter(|&entry| !index.is_removed(entry));
        let mut listed: Vec<_> = kept
            .map(|entry| (index.path(entry), index.is_dir(entry)))
            .collect();
        listed.sort();
        listed
    }

    /// Saved while patches keep coming, the file opens to the list in use, at the event of the
    /// last patch.
    #[test]
    fn a_list_patched_in_place_is_saved_with_its_own_event() {
        let (_h, home) = home_with(&["notes/a.txt", "work/plan.md"]);
        let support = tempfile::tempdir().unwrap();
        let file = support.path().join("index");
        let live = saving_live(&file, Duration::from_millis(1), Duration::from_millis(5));
        let events = Events::default();
        let notify = notify_to(&events);
        let mut source = home_source(&home, &live, &notify);
        let notes = home.join("notes");
        let (swaps, _swapped) = mpsc::channel();
        with_saver(&live, &events, || {
            for event_id in 1..=30 {
                std::fs::write(notes.join(format!("{event_id}.txt")), "").unwrap();
                if event_id % 3 == 0 {
                    std::fs::remove_file(notes.join(format!("{}.txt", event_id - 2))).unwrap();
                }
                let pass = patch(vec![notes.clone()], event_id);
                rebuild(&mut source, pass, &live, &swaps, &notify);
            }
            wait_until("the last save", || saved_event(&file) == Some(30));
        });
        let (settings, _) = source.saving().unwrap();
        let saved = open_index(&file, &settings).unwrap();
        let list = live.newest.lock().unwrap().clone().unwrap().1;
        assert_eq!(read(&list).unwrap().version, 30);
        let live_listing = listing(&read(&list).unwrap().index);
        assert_eq!(listing(&saved.scan.index), live_listing);
        assert_eq!(saved.event_id, 30);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    fn index_of(files: &[&str]) -> (tempfile::TempDir, Index) {
        let (dir, home) = home_with(files);
        let ignore = Ignore::default();
        let scan = argus_core::scan(&home, &home, Clutter::Skip, &ignore, Access::Full);
        (dir, scan.unwrap().index)
    }

    #[test]
    fn a_search_sent_before_a_swap_uses_the_old_list_and_one_after_the_new() {
        let (_old_dir, old) = index_of(&["a.txt"]);
        let (_new_dir, new) = index_of(&["a.txt", "budget.txt"]);
        let (_newer_dir, newer) = index_of(&["a.txt", "budget.txt", "budget 2.txt"]);
        let new_len = new.entry_count() as u64;
        let newer_len = newer.entry_count() as u64;
        let events = Events::default();
        let notify = Notify {
            ctx: &events as *const Events as *mut c_void,
            on_event: record,
        };
        let searcher = Searcher::with_threads(1);
        let done = Done::default();
        let found = |done: &Done| done.lock().unwrap().take().unwrap().1.rows.len();

        let (jobs, inbox) = mpsc::channel();
        let before = searcher.ticket();
        let query = "budget".to_owned();
        jobs.send(Job::Search {
            ticket: before,
            query: query.clone(),
            folder: None,
        })
        .unwrap();
        jobs.send(Job::Swap {
            list: list_of(new),
            took: 7,
        })
        .unwrap();
        drop(jobs);
        let (old, opens) = (list_of(old), Mutex::default());
        serve(old, &searcher, &inbox, &done, &opens, &notify, RERANK);
        assert_eq!(found(&done), 0);
        let mut seen = std::mem::take(&mut *events.seen.lock().unwrap());
        assert_eq!(
            seen,
            [
                (TL_EVENT_READY, before.number()),
                (TL_EVENT_REBUILT, 7 << 32 | new_len)
            ]
        );

        let (jobs, inbox) = mpsc::channel();
        let after = searcher.ticket();
        jobs.send(Job::Swap {
            list: list_of(newer),
            took: 9,
        })
        .unwrap();
        jobs.send(Job::Search {
            ticket: after,
            query,
            folder: None,
        })
        .unwrap();
        drop(jobs);
        let (_dir, unused) = index_of(&[]);
        let unused = list_of(unused);
        serve(unused, &searcher, &inbox, &done, &opens, &notify, RERANK);
        assert_eq!(found(&done), 2);
        seen = std::mem::take(&mut *events.seen.lock().unwrap());
        assert_eq!(
            seen,
            [
                (TL_EVENT_REBUILT, 9 << 32 | newer_len),
                (TL_EVENT_READY, after.number())
            ]
        );
    }

    /// Rows as the window shows them: name and folder joined back into a path.
    fn found_paths(core: &Core, events: &Events, query: &str) -> Vec<PathBuf> {
        let ticket = submit(core, query);
        events.wait_for(TL_EVENT_READY, ticket);
        let rows = take(core, ticket).unwrap();
        rows.into_iter()
            .map(|(name, folder, _)| Path::new(&folder).join(name))
            .collect()
    }

    fn opened(core: &Core, path: &Path) -> u32 {
        let path = path.to_str().unwrap();
        unsafe { tl_opened(core.0, path.as_ptr(), path.len()) }
    }

    fn frecency_open(core: &Core, file: &Path) -> u32 {
        let file = file.to_str().unwrap();
        unsafe { tl_frecency_open(core.0, file.as_ptr(), file.len()) }
    }

    #[test]
    fn a_file_opened_ranks_first_from_the_next_search() {
        let events = Events::default();
        let core = start(2_000, &events);
        let before = found_paths(&core, &events, "budget");
        let last = before.last().unwrap();
        assert_eq!(opened(&core, last), 1);
        assert_eq!(opened(&core, last), 1);
        let after = found_paths(&core, &events, "budget");
        assert_eq!(after[0], *last);
        assert_eq!(after[1..], before[..ROWS - 1]);
    }

    #[test]
    fn opens_are_saved_and_rank_again_in_the_next_core() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("io.example/frecency");
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(frecency_open(&core, &file), 0);
        let last = found_paths(&core, &events, "budget").pop().unwrap();
        assert_eq!(opened(&core, &last), 1);
        drop(core);
        assert_eq!(Frecency::open(&file).unwrap().len(), 1);

        let events = Events::default();
        let core = start(2_000, &events);
        assert_ne!(found_paths(&core, &events, "budget")[0], last);
        assert_eq!(frecency_open(&core, &file), 1);
        assert_eq!(found_paths(&core, &events, "budget")[0], last);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_damaged_list_is_told_and_starts_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let mut list = Frecency::default();
        list.record(Path::new("/stand-in/old.md"), unix_now());
        list.save(&file).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 1]).unwrap();
        let events = Events::default();
        let core = start(100, &events);
        assert_eq!(frecency_open(&core, &file), 0);
        let notes = events.wait_for_note("could not be read and starts again");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(opened(&core, Path::new("/stand-in/a.md")), 1);
        assert_eq!(Frecency::open(&file).unwrap().len(), 1);
    }

    /// Another kind of file, or a list from a newer build, is never saved over.
    #[test]
    fn a_list_it_cannot_read_is_told_and_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let events = Events::default();
        let core = start(2_000, &events);
        let last = found_paths(&core, &events, "budget").pop().unwrap();
        let mut newer = Vec::from(*b"TL-OPENS");
        newer.extend_from_slice(&2_u32.to_le_bytes());
        newer.extend_from_slice(&[0; 12]);
        for kept in [b"not a list of opened files".as_slice(), &newer] {
            std::fs::write(&file, kept).unwrap();
            assert_eq!(frecency_open(&core, &file), 0);
            assert_eq!(opened(&core, &last), 1);
            assert_eq!(found_paths(&core, &events, "budget")[0], last);
            assert_eq!(std::fs::read(&file).unwrap(), kept);
        }
        let notes = events.notes.lock().unwrap().clone();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("is left as it is: not an Argus list"));
        assert!(notes[1].contains("is left as it is: saved as version 2"));
    }

    #[test]
    fn an_open_that_cannot_be_saved_is_told_and_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(frecency_open(&core, &dir.path().join("taken/frecency")), 0);
        std::fs::write(dir.path().join("taken"), "").unwrap();
        let last = found_paths(&core, &events, "budget").pop().unwrap();
        assert_eq!(opened(&core, &last), 1);
        events.wait_for_note("could not be saved");
        assert_eq!(found_paths(&core, &events, "budget")[0], last);
    }

    /// The list read replaces every open counted before, and reading it again changes nothing.
    #[test]
    fn a_list_read_replaces_the_opens_counted_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let events = Events::default();
        let core = start(2_000, &events);
        let before = found_paths(&core, &events, "budget");
        let (early, saved) = (&before[ROWS - 1], &before[ROWS - 2]);
        assert_eq!(opened(&core, early), 1);
        assert_eq!(found_paths(&core, &events, "budget")[0], *early);
        let mut list = Frecency::default();
        list.record(saved, unix_now());
        list.save(&file).unwrap();
        for _ in 0..2 {
            assert_eq!(frecency_open(&core, &file), 1);
            let after = found_paths(&core, &events, "budget");
            assert_eq!(after[0], *saved);
            assert!(!after[..ROWS - 1].contains(early), "{after:?}");
        }
        assert!(events.notes.lock().unwrap().is_empty());
    }

    /// Opens from several threads at once each count, and the file ends up with all of them.
    #[test]
    fn opens_from_several_threads_all_count_and_are_all_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frecency");
        let events = Events::default();
        let core = start(2_000, &events);
        assert_eq!(frecency_open(&core, &file), 0);
        let found = found_paths(&core, &events, "budget");
        let shared = core.0 as usize;
        thread::scope(|threads| {
            for paths in found.chunks(10) {
                threads.spawn(move || {
                    for path in paths {
                        let path = path.to_str().unwrap();
                        let core = shared as *const TlCore;
                        assert_eq!(unsafe { tl_opened(core, path.as_ptr(), path.len()) }, 1);
                    }
                });
            }
        });
        let saved = Frecency::open(&file).unwrap();
        assert_eq!(saved.len(), found.len());
        let now = unix_now();
        assert!(found.iter().all(|path| saved.score(path, now) > 0.9));
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn refuses_paths_and_files_it_cannot_use() {
        let events = Events::default();
        let core = start(100, &events);
        let broken = [b'/', 0xFF];
        unsafe {
            assert_eq!(tl_opened(core.0, broken.as_ptr(), broken.len()), 0);
            assert_eq!(tl_opened(core.0, std::ptr::null(), 0), 0);
            assert_eq!(tl_frecency_open(core.0, broken.as_ptr(), broken.len()), 0);
            assert_eq!(tl_frecency_open(core.0, std::ptr::null(), 0), 0);
        }
        let long = format!("/{}", "a".repeat(4096));
        assert_eq!(opened(&core, Path::new(&long)), 0);
        assert!(events.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_rebuilt_list_is_ranked_by_the_opens_before_its_first_search() {
        let (_old_dir, old) = index_of(&["plan.md"]);
        let (_new_dir, new) = index_of(&["plan.md", "a/b/plan.md"]);
        let deep = new
            .ids()
            .map(|entry| new.path(entry))
            .find(|path| path.ends_with("a/b/plan.md"))
            .unwrap();
        let mut frecency = Frecency::default();
        frecency.record(&deep, unix_now());
        let opens = Mutex::new(Opens {
            frecency,
            file: None,
        });
        let events = Events::default();
        let notify = Notify {
            ctx: &events as *const Events as *mut c_void,
            on_event: record,
        };
        let searcher = Searcher::with_threads(1);
        let done = Done::default();
        let (jobs, inbox) = mpsc::channel();
        jobs.send(Job::Swap {
            list: list_of(new),
            took: 1,
        })
        .unwrap();
        let ticket = searcher.ticket();
        let query = "plan".to_owned();
        let folder = None;
        jobs.send(Job::Search {
            ticket,
            query,
            folder,
        })
        .unwrap();
        drop(jobs);
        let old = list_of(old);
        serve(old, &searcher, &inbox, &done, &opens, &notify, RERANK);
        let (_, packed) = done.lock().unwrap().take().unwrap();
        let row = &packed.rows[0];
        let folder = row.folder_start as usize..(row.folder_start + row.folder_len) as usize;
        assert_eq!(Path::new(&packed.text[folder]), deep.parent().unwrap());
    }

    /// Scores fade, so boosts older than `rerank` are worked out again before the next search,
    /// with no open or new list since.
    #[test]
    fn old_boosts_are_worked_out_again_before_a_search() {
        let (_dir, index) = index_of(&["plan.md", "a/b/plan.md"]);
        let deep = index
            .ids()
            .map(|entry| index.path(entry))
            .find(|path| path.ends_with("a/b/plan.md"))
            .unwrap();
        let list = list_of(index);
        let events = Events::default();
        let notify = Notify {
            ctx: &events as *const Events as *mut c_void,
            on_event: record,
        };
        let searcher = Searcher::with_threads(1);
        for (rerank, climbs) in [(RERANK, false), (Duration::ZERO, true)] {
            let opens = Mutex::<Opens>::default();
            let done = Done::default();
            let (jobs, inbox) = mpsc::channel();
            let search = |jobs: &Sender<Job>| {
                let ticket = searcher.ticket();
                let (query, folder) = ("plan".to_owned(), None);
                jobs.send(Job::Search {
                    ticket,
                    query,
                    folder,
                })
                .unwrap();
                events.wait_for(TL_EVENT_READY, ticket.number());
            };
            thread::scope(|threads| {
                let (list, searcher, done, opens) = (Arc::clone(&list), &searcher, &done, &opens);
                threads.spawn(move || serve(list, searcher, &inbox, done, opens, &notify, rerank));
                search(&jobs);
                // Behind the worker's back, as an hour of fading would be.
                opens.lock().unwrap().frecency.record(&deep, unix_now());
                search(&jobs);
                drop(jobs);
            });
            let (_, packed) = done.lock().unwrap().take().unwrap();
            let row = &packed.rows[0];
            let folder = row.folder_start as usize..(row.folder_start + row.folder_len) as usize;
            let first = Path::new(&packed.text[folder]);
            assert_eq!(first == deep.parent().unwrap(), climbs, "{first:?}");
        }
    }

    /// Each row's folder and name joined back into a path.
    fn paths_of(packed: &Packed) -> Vec<PathBuf> {
        let text = |start: u32, len: u32| &packed.text[start as usize..(start + len) as usize];
        packed
            .rows
            .iter()
            .map(|row| {
                let folder = text(row.folder_start, row.folder_len);
                Path::new(folder).join(text(row.name_start, row.name_len))
            })
            .collect()
    }

    /// A search sent while the list is patched waits, then answers from the patched list:
    /// the folder searched inside and the boosts are worked out again, as the patch numbered
    /// the entries anew and at times left fewer than before.
    #[test]
    fn searches_sent_during_a_patch_answer_from_the_patched_list() {
        let files = [
            "notes/keep.md",
            "notes/r0.md",
            "plan.md",
            "deep/a/b/plan.md",
        ];
        let (_dir, home) = home_with(&files);
        let (notes, other, deep) = (
            home.join("notes"),
            home.join("other"),
            home.join("deep/a/b"),
        );
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let list = list_of(settings.scan().unwrap().index);
        let mut frecency = Frecency::default();
        frecency.record(&deep.join("plan.md"), unix_now());
        let opens = Mutex::new(Opens {
            frecency,
            file: None,
        });
        let events = Events::default();
        let notify = notify_to(&events);
        let searcher = Searcher::with_threads(1);
        let done = Done::default();
        let (jobs, inbox) = mpsc::channel();
        thread::scope(|threads| {
            let (worker_list, searcher, done, opens) =
                (Arc::clone(&list), &searcher, &done, &opens);
            threads
                .spawn(move || serve(worker_list, searcher, &inbox, done, opens, &notify, RERANK));
            let ask = |query: &str, folder: Option<&Path>| {
                let ticket = searcher.ticket();
                let query = query.to_owned();
                let folder = folder.map(|folder| folder.to_string_lossy().into_owned());
                jobs.send(Job::Search {
                    ticket,
                    query,
                    folder,
                })
                .unwrap();
                events.wait_for(TL_EVENT_READY, ticket.number());
                paths_of(&done.lock().unwrap().take().unwrap().1)
            };
            // Works out the folder and the boosts on the list as first read.
            assert_eq!(ask("plan", None)[0], deep.join("plan.md"));
            assert_eq!(ask("md", Some(&notes)).len(), 2);
            for round in 1..=40 {
                let renamed = notes.join(format!("r{round}.md"));
                std::fs::rename(notes.join(format!("r{}.md", round - 1)), &renamed).unwrap();
                if round % 2 == 1 {
                    for name in 0..8 {
                        std::fs::create_dir_all(other.join(format!("{name}"))).unwrap();
                    }
                } else {
                    std::fs::remove_dir_all(&other).unwrap();
                }
                let (locked, patching) = mpsc::channel();
                let (list, settings) = (&list, &settings);
                let changed = [notes.as_path(), home.as_path()];
                let patch = threads.spawn(move || {
                    let mut listed = list.write().unwrap();
                    locked.send(()).unwrap();
                    // Long enough that the searches below are sent while it runs.
                    thread::sleep(Duration::from_millis(5));
                    let update = settings.update(&mut listed.index, &changed);
                    assert!(update.compacted);
                    listed.version += 1;
                });
                patching.recv().unwrap();
                let mut inside = ask("md", Some(&notes));
                inside.sort();
                assert_eq!(inside, [notes.join("keep.md"), renamed], "round {round}");
                let plans = ask("plan", None);
                assert_eq!(plans.len(), 2, "round {round}");
                assert!(plans[0].starts_with(&deep), "round {round}: {plans:?}");
                patch.join().unwrap();
            }
            drop(jobs);
        });
    }

    /// A line as the window would copy it: the file's path, the line number, the line, and
    /// the part of it that matched.
    type Line = (String, u64, String, String);

    fn grep(core: &Core, folders: &[&Path], query: &str) -> u64 {
        let mut bytes = Vec::new();
        for folder in folders {
            bytes.extend_from_slice(folder.to_str().unwrap().as_bytes());
            bytes.push(0);
        }
        let (folders, len) = (bytes.as_ptr(), bytes.len());
        unsafe { tl_grep_submit(core.0, folders, len, query.as_ptr(), query.len()) }
    }

    /// The lines waiting for `ticket`, and the counts once the search is over.
    fn take_lines(core: &Core, ticket: u64) -> Option<(Vec<Line>, Option<Report>)> {
        let mut out = std::mem::MaybeUninit::<TlLines>::uninit();
        if unsafe { tl_grep_take(core.0, ticket, out.as_mut_ptr()) } != TL_OK {
            return None;
        }
        let lines = unsafe { out.assume_init() };
        let text = |start: u32, len: u32| {
            let bytes =
                unsafe { std::slice::from_raw_parts(lines.text.add(start as usize), len as usize) };
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        let copied = unsafe { std::slice::from_raw_parts(lines.lines, lines.count) }
            .iter()
            .map(|line| {
                let shown = text(line.text_start, line.text_len);
                let matched =
                    &shown.as_bytes()[line.match_start as usize..][..line.match_len as usize];
                let matched = String::from_utf8(matched.to_vec()).unwrap();
                (
                    text(line.path_start, line.path_len),
                    line.line,
                    shown,
                    matched,
                )
            })
            .collect();
        let counts = &lines.report;
        let report = Report {
            files_searched: counts.files_searched as usize,
            lines_found: counts.lines_found as usize,
            binary: counts.binary as usize,
            too_large: counts.too_large as usize,
            cloud_only: counts.cloud_only as usize,
            unreadable: counts.unreadable as usize,
        };
        if !lines.done {
            assert_eq!(report, Report::default(), "counts before the end");
        }
        let report = lines.done.then_some(report);
        unsafe { tl_lines_free(lines) };
        Some((copied, report))
    }

    /// Takes each batch `ticket` announces until its end, and returns the lines sorted.
    fn grep_all(core: &Core, events: &Events, ticket: u64) -> (Vec<Line>, Report) {
        grep_rest(core, events, ticket, 0)
    }

    /// As `grep_all`, once `taken` batches were taken.
    fn grep_rest(core: &Core, events: &Events, ticket: u64, taken: usize) -> (Vec<Line>, Report) {
        let mut found = Vec::new();
        for batch in taken + 1.. {
            events.wait_for_times(TL_EVENT_LINES, ticket, batch);
            let (lines, report) = take_lines(core, ticket).expect("a batch for each event");
            found.extend(lines);
            if let Some(report) = report {
                found.sort();
                return (found, report);
            }
        }
        unreachable!()
    }

    fn write(path: &Path, text: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A core over `home`, so its text searches read that folder's ignore list, never yours.
    fn text_core(home: &Path, full_disk: u32, events: &Events) -> Core {
        let core = scan(home.to_str().unwrap(), full_disk, events);
        events.wait_for_event(TL_EVENT_INDEXED);
        core
    }

    #[test]
    fn a_report_keeps_each_count_in_its_field() {
        let report = TlReport::from(Report {
            files_searched: 1,
            lines_found: 2,
            binary: 3,
            too_large: 4,
            cloud_only: 5,
            unreadable: 6,
        });
        let counts = [
            report.files_searched,
            report.lines_found,
            report.binary,
            report.too_large,
            report.cloud_only,
            report.unreadable,
        ];
        assert_eq!(counts, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn freeing_nothing_taken_is_harmless() {
        unsafe {
            tl_lines_free(std::mem::zeroed());
            tl_rows_free(std::mem::zeroed());
        }
    }

    #[test]
    fn a_text_search_comes_back_as_lines_with_its_counts() {
        let (_dir, home) = home_with(&[]);
        let notes = home.join("notes");
        write(
            &notes.join("plan.md"),
            "Budget for 2026\nnothing here\n  Café budget ☕  \n",
        );
        write(&notes.join("old/budget.txt"), "no match\n");
        write(&home.join("photo.png"), b"\x89PNG\0budget");
        write(&home.join("node_modules/budget.js"), "budget\n");
        let events = Events::default();
        let core = text_core(&home, 1, &events);

        // A folder inside another, or named twice, is searched once.
        let ticket = grep(&core, &[&notes, &home, &notes.join("old"), &home], "budget");
        assert_ne!(ticket, 0);
        let (lines, report) = grep_all(&core, &events, ticket);
        let plan = notes.join("plan.md").to_string_lossy().into_owned();
        let line = |number, shown: &str, matched: &str| {
            (plan.clone(), number, shown.to_owned(), matched.to_owned())
        };
        assert_eq!(
            lines,
            [
                line(1, "Budget for 2026", "Budget"),
                line(3, "Café budget ☕", "budget"),
            ]
        );
        let expected = Report {
            files_searched: 2,
            lines_found: 2,
            binary: 1,
            ..Report::default()
        };
        assert_eq!(report, expected);
        assert_eq!(take_lines(&core, ticket), None);
    }

    #[test]
    fn a_text_search_follows_the_ignore_list() {
        let (_dir, home) = home_with(&[]);
        write(&home.join(".config/argus/ignore"), "secret\n");
        write(&home.join("secret/a.txt"), "budget\n");
        write(&home.join("a.txt"), "budget\n");
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        let ticket = grep(&core, &[&home], "budget");
        let (lines, _) = grep_all(&core, &events, ticket);
        let shown: Vec<&str> = lines.iter().map(|line| line.0.as_str()).collect();
        assert_eq!(shown, [home.join("a.txt").to_str().unwrap()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_named_by_a_link_is_searched_once_by_its_real_path() {
        let (_dir, home) = home_with(&[]);
        let notes = home.join("notes");
        write(&notes.join("a.txt"), "budget\n");
        std::os::unix::fs::symlink(&notes, home.join("link")).unwrap();
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        let ticket = grep(&core, &[&home.join("link"), &notes], "budget");
        let (lines, report) = grep_all(&core, &events, ticket);
        let shown: Vec<&str> = lines.iter().map(|line| line.0.as_str()).collect();
        assert_eq!(shown, [notes.join("a.txt").to_str().unwrap()]);
        assert_eq!(report.files_searched, 1);
    }

    #[test]
    fn a_name_search_returns_while_a_text_search_runs() {
        let (_dir, home) = home_with(&[]);
        write(&home.join("a.txt"), "budget\n");
        write(&home.join("b.txt"), "budget\n");
        let events = Events::default();
        let core = text_core(&home, 1, &events);

        events.hold_lines(true);
        let text = grep(&core, &[&home], "budget");
        // The text search now waits inside `on_event`, halfway through its files.
        let halfway = events.heard(TL_EVENT_LINES, text);
        let name = submit(&core, "txt");
        let answered = events.heard(TL_EVENT_READY, name);
        let rows = take(&core, name);
        // However many lines wait, the window is told once until it takes them.
        let told = events.count(TL_EVENT_LINES, text);
        let so_far = take_lines(&core, text);
        events.hold_lines(false);

        assert!(halfway && answered);
        assert_eq!(told, 1);
        assert!(rows.is_some_and(|rows| !rows.is_empty()));
        let (first, end) = so_far.unwrap();
        assert!(!first.is_empty());
        assert_eq!(end, None, "the text search is still running");
        let (rest, report) = grep_rest(&core, &events, text, 1);
        assert_eq!(first.len() + rest.len(), 2);
        assert_eq!(report.lines_found, 2);
    }

    #[test]
    fn a_cancelled_text_search_stops_and_keeps_nothing() {
        let (_dir, home) = home_with(&[]);
        for file in ["a", "b", "c", "d", "e"] {
            write(&home.join(format!("{file}.txt")), "budget\n".repeat(50));
        }
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        // Harmless before any text search.
        unsafe { tl_grep_cancel(core.0) };
        assert_eq!(take_lines(&core, 0), None);
        assert_eq!(take_lines(&core, 1), None);

        events.hold_lines(true);
        let cancelled = grep(&core, &[&home], "budget");
        let halfway = events.heard(TL_EVENT_LINES, cancelled);
        unsafe { tl_grep_cancel(core.0) };
        let kept = take_lines(&core, cancelled);
        events.hold_lines(false);
        assert!(halfway);
        assert_eq!(kept, None);

        // The worker takes one search at a time, so this one ends after the cancelled one.
        let next = grep(&core, &[&home], "budget");
        let (lines, report) = grep_all(&core, &events, next);
        assert_eq!((lines.len(), report.lines_found), (LINES, LINES));
        assert_eq!(
            events.count(TL_EVENT_LINES, cancelled),
            1,
            "nothing after the cancel"
        );
        assert_eq!(take_lines(&core, cancelled), None);
    }

    #[test]
    fn a_text_search_stops_at_its_lines_across_folders() {
        let (_dir, home) = home_with(&[]);
        let (one, two) = (home.join("one"), home.join("two"));
        write(&one.join("a.txt"), "budget\n".repeat(80));
        write(&two.join("b.txt"), "budget\n".repeat(80));
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        let ticket = grep(&core, &[&one, &two], "budget");
        let (lines, report) = grep_all(&core, &events, ticket);
        assert_eq!((lines.len(), report.lines_found), (LINES, LINES));
        assert_eq!(report.files_searched, 2);
    }

    #[test]
    fn a_folder_after_the_last_line_is_never_listed() {
        let (_dir, home) = home_with(&[]);
        let one = home.join("one");
        write(&one.join("a.txt"), "budget\n".repeat(LINES));
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        // Listed, the missing folder would count as unreadable.
        let ticket = grep(&core, &[&one, &home.join("two")], "budget");
        let full = Report {
            files_searched: 1,
            lines_found: LINES,
            ..Report::default()
        };
        assert_eq!(grep_all(&core, &events, ticket).1, full);
    }

    #[test]
    fn only_the_newest_text_search_answers() {
        let (_dir, home) = home_with(&[]);
        write(&home.join("a.txt"), "b\nbu\nbud\nbudg\nbudge\nbudget\n");
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        // The first search waits at its first line while the rest are typed.
        events.hold_lines(true);
        let first = grep(&core, &[&home], "b");
        let held = events.heard(TL_EVENT_LINES, first);
        let typed = ["bu", "bud", "budg", "budge", "budget"];
        let tickets: Vec<u64> = typed
            .iter()
            .map(|query| grep(&core, &[&home], query))
            .collect();
        events.hold_lines(false);
        assert!(held);

        let last = *tickets.last().unwrap();
        let (lines, _) = grep_all(&core, &events, last);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].1, 6);
        // Those typed while the first ran were skipped unread.
        for &skipped in &tickets[..tickets.len() - 1] {
            assert_eq!(events.count(TL_EVENT_LINES, skipped), 0);
        }
        assert_eq!(events.count(TL_EVENT_LINES, first), 1);
    }

    #[test]
    fn a_text_search_counts_folders_it_cannot_list_and_refuses_bad_paths() {
        let (_dir, home) = home_with(&[]);
        let notes = home.join("notes");
        write(&notes.join("a.txt"), "budget\n");
        let events = Events::default();
        let core = text_core(&home, 1, &events);

        // One bad path refuses the whole list, and leaves the search under way alone.
        events.hold_lines(true);
        let running = grep(&core, &[&notes], "budget");
        let held = events.heard(TL_EVENT_LINES, running);
        let mut folders = notes.to_str().unwrap().as_bytes().to_vec();
        folders.extend_from_slice(&[0, 0xff, 0]);
        let query = "budget";
        let refused =
            unsafe { tl_grep_submit(core.0, folders.as_ptr(), folders.len(), query.as_ptr(), 6) };
        events.hold_lines(false);
        assert!(held);
        assert_eq!(refused, 0);
        assert_eq!(grep_all(&core, &events, running).1.lines_found, 1);

        let gone = home.join("gone");
        let file = notes.join("a.txt");
        let ticket = grep(&core, &[&gone, &file], "budget");
        let unreadable = Report {
            unreadable: 2,
            ..Report::default()
        };
        assert_eq!(grep_all(&core, &events, ticket), (Vec::new(), unreadable));

        let ticket = grep(&core, &[&notes, &gone], "budget");
        let both = Report {
            files_searched: 1,
            lines_found: 1,
            unreadable: 1,
            ..Report::default()
        };
        let (lines, report) = grep_all(&core, &events, ticket);
        assert_eq!((lines.len(), report), (1, both));

        // An empty box, two lines, or no folders, end at once with nothing, listing nothing.
        for query in ["", "budget\nbudget"] {
            let ticket = grep(&core, &[&gone], query);
            let ended = grep_all(&core, &events, ticket);
            assert_eq!(ended, (Vec::new(), Report::default()), "{query:?}");
        }
        let null = std::ptr::null();
        let nowhere = unsafe { tl_grep_submit(core.0, null, 0, null, 0) };
        assert_ne!(nowhere, 0);
        assert_eq!(
            grep_all(&core, &events, nowhere),
            (Vec::new(), Report::default())
        );
        // Text that isn't UTF-8 is read as Swift would show it.
        let folder = format!("{}\0", notes.to_str().unwrap());
        let broken =
            unsafe { tl_grep_submit(core.0, folder.as_ptr(), folder.len(), [0xff].as_ptr(), 1) };
        assert_ne!(broken, 0);
        assert_eq!(grep_all(&core, &events, broken).1.files_searched, 1);
    }

    #[test]
    fn text_search_reads_guarded_folders_only_with_full_disk_access() {
        let (_dir, home) = home_with(&[]);
        let documents = home.join("Documents");
        write(&documents.join("budget.txt"), "budget\n");
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 0, &events);
        events.wait_for(TL_EVENT_INDEXED, 1);

        let limited = grep(&core, &[&home], "budget");
        assert_eq!(grep_all(&core, &events, limited).1.lines_found, 0);
        // Named directly, a guarded folder still stays unread.
        let named = grep(&core, &[&documents, &documents.join("work")], "budget");
        let unread = Report {
            unreadable: 1,
            ..Report::default()
        };
        assert_eq!(grep_all(&core, &events, named), (Vec::new(), unread));
        unsafe { tl_grant_full_disk(core.0, 0, 1) };
        let full = grep(&core, &[&home], "budget");
        assert_eq!(grep_all(&core, &events, full).1.lines_found, 1);
    }

    #[test]
    fn a_stand_in_core_reads_guarded_folders_only_once_granted() {
        let events = Events::default();
        let core = start(100, &events);
        let text = unsafe { &*core.0 }.text.as_ref().unwrap();
        assert!(!text.full_disk.load(Ordering::Relaxed));
        unsafe { tl_grant_full_disk(core.0, 0, 1) };
        assert!(text.full_disk.load(Ordering::Relaxed));
    }

    #[test]
    fn freeing_the_core_stops_its_text_search() {
        let (_dir, home) = home_with(&[]);
        for file in 0..50 {
            write(&home.join(format!("{file}.txt")), "budget\n".repeat(10));
        }
        let events = Events::default();
        let core = text_core(&home, 1, &events);
        events.hold_lines(true);
        let ticket = grep(&core, &[&home], "budget");
        let held = events.heard(TL_EVENT_LINES, ticket);
        let search = unsafe { &*core.0 }.text.as_ref().unwrap().started.get();
        let grep = Arc::clone(&search.unwrap().grep);
        let core = std::mem::ManuallyDrop::new(core).0 as usize;
        let freeing = std::thread::spawn(move || unsafe { tl_core_free(core as *mut TlCore) });
        let deadline = Instant::now() + Duration::from_secs(10);
        while grep.newest() == ticket && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let waited = !freeing.is_finished();
        events.hold_lines(false);
        freeing.join().unwrap();
        assert!(held && waited, "free waits for the text search");
        // It gave up at once: no more lines and no end, then or later.
        assert_eq!(events.count(TL_EVENT_LINES, ticket), 1);
        let heard = events.seen.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(events.seen.lock().unwrap().len(), heard);
    }

    #[test]
    fn a_bug_ends_only_its_own_text_search() {
        let events = Events::default();
        let ctx = &events as *const Events as *mut c_void;
        let notify = Notify {
            ctx,
            on_event: record,
        };
        let (grep, waiting) = (Grep::with_threads(1), Mutex::new(Waiting::default()));
        let (grep, waiting) = (&grep, &waiting);
        let finder = |find: &Find| {
            if find.query == "broken" {
                let line = Found {
                    entry: 1,
                    path: Path::new("a.txt"),
                    line: 1,
                    text: "broken",
                    matched: 0..6,
                };
                add(waiting, grep, find.ticket.number(), &line, notify);
                add(waiting, grep, find.ticket.number(), &line, notify);
                panic!("a bug in a text search");
            }
            Some(Report {
                files_searched: 1,
                ..Report::default()
            })
        };
        let ended = |number: u64| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let waiting = waiting.lock().unwrap();
                if let Some(report) = waiting.report
                    && waiting.ticket == number
                {
                    return (waiting.batch.lines.len(), report, waiting.failed);
                }
                drop(waiting);
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("search {number} never ended");
        };
        let (jobs, inbox) = mpsc::channel();
        std::thread::scope(|scope| {
            // Owned here, so a failed check closes the queue and the loop ends.
            let jobs = jobs;
            scope.spawn(move || find_loop(grep, &inbox, waiting, notify, finder));
            let send = |query: &str| {
                let ticket = grep.ticket();
                let (folders, query) = (Vec::new(), query.to_owned());
                let access = Access::Full;
                let find = Find {
                    ticket,
                    folders,
                    query,
                    access,
                };
                jobs.send(find).unwrap();
                ticket.number()
            };
            let broken = send("broken");
            let found_two = Report {
                lines_found: 2,
                ..Report::default()
            };
            assert_eq!(ended(broken), (2, found_two, true));
            assert_eq!(events.count(TL_EVENT_LINES, broken), 1);

            let next = send("fine");
            let searched = Report {
                files_searched: 1,
                ..Report::default()
            };
            assert_eq!(ended(next), (0, searched, false));
            assert_eq!(events.count(TL_EVENT_LINES, next), 1);
        });
    }

    #[test]
    fn each_folder_is_searched_once() {
        // Names that sort before "/" byte by byte must not split a folder from its insides.
        let names = [
            "/a", "/a/b", "/a/b/c", "/ab", "/x", "/a/b2", "/a-b", "/a.b", "/a b",
        ];
        // Under a root that never exists, so no name resolves to a real folder (CI on
        // Windows has a D:\a).
        let paths: Vec<PathBuf> = names
            .iter()
            .map(|name| PathBuf::from(format!("/argus-no-such-root{name}")))
            .collect();
        // Every set of these folders, each also named twice.
        for set in 0..1_u32 << names.len() {
            let chosen: Vec<PathBuf> = (0..names.len())
                .filter(|bit| set & (1 << bit) != 0)
                .flat_map(|bit| [paths[bit].clone(), paths[bit].clone()])
                .rev()
                .collect();
            let kept = outermost(&chosen);
            for folder in &chosen {
                let outer = kept.iter().filter(|outer| folder.starts_with(outer));
                assert_eq!(outer.count(), 1, "{folder:?} in {kept:?}");
            }
            assert!(kept.iter().all(|folder| chosen.contains(folder)));
        }
    }

    /// Name search times while a text search over `TL_GREP_FOLDER` (the home folder unless
    /// set, without the guarded folders) starts again with each keystroke, 100 ms apart, as
    /// live typing does. Run with
    /// `cargo test --release -p argus-ffi -- --ignored --nocapture name_search_times`.
    #[test]
    #[ignore = "builds a million names and lists a whole folder; slow outside --release"]
    fn name_search_times_while_text_search_runs() {
        let folder =
            std::env::var_os("TL_GREP_FOLDER").map_or_else(argus_core::home, PathBuf::from);
        let events = Events::default();
        let core = start(1_000_000, &events);
        let queries = [
            "argus plan",
            "budget notes",
            "readme",
            "会議 report",
            "main.rs",
        ];
        for busy in [false, true, false, true] {
            let mut times = Vec::new();
            for key in 0..60 {
                let pressed = Instant::now();
                if busy {
                    // Matches nothing, so each search would list and read the whole folder.
                    grep(&core, &[&folder], &format!("zq{key} no such text"));
                }
                for query in queries {
                    let started = Instant::now();
                    let ticket = submit(&core, query);
                    events.wait_for(TL_EVENT_READY, ticket);
                    times.push(started.elapsed());
                    assert!(take(&core, ticket).is_some());
                }
                std::thread::sleep(Duration::from_millis(100).saturating_sub(pressed.elapsed()));
            }
            unsafe { tl_grep_cancel(core.0) };
            std::thread::sleep(Duration::from_millis(500));
            times.sort();
            let at = |share: usize| times[(times.len() - 1) * share / 100];
            println!(
                "{}: p50 {:?}, p95 {:?}, max {:?} over {} name searches",
                if busy {
                    "text search per keystroke"
                } else {
                    "idle"
                },
                at(50),
                at(95),
                at(100),
                times.len()
            );
        }
        println!("text searches over {}", folder.display());
    }

    /// What the rebuild thread did next, on a clock the test moves by hand.
    #[derive(Debug, PartialEq)]
    enum Step {
        /// Waiting for a change, or for this much more time to pass.
        Waits(Option<Duration>),
        /// A rebuild began this far into the test, and holds until the next step. `clutter`
        /// is `None` for one that only changed volumes or read folders again.
        Starts {
            at: Duration,
            full_disk: bool,
            mark: u64,
            clutter: Option<Clutter>,
        },
    }

    fn starts(ms: u64, mark: u64) -> Step {
        Step::Starts {
            at: Duration::from_millis(ms),
            full_disk: false,
            mark,
            clutter: Some(Clutter::Skip),
        }
    }

    fn rereads(ms: u64, mark: u64) -> Step {
        Step::Starts {
            at: Duration::from_millis(ms),
            full_disk: false,
            mark,
            clutter: None,
        }
    }

    /// Time stands still until the test moves it, and the rebuild thread goes on only when
    /// the test says, so each step is the same on a busy Mac as on an idle one.
    struct Manual {
        zero: Instant,
        at: Mutex<Duration>,
        steps: Sender<Step>,
        go: Mutex<Receiver<()>>,
        /// The event id each rebuild's list is current to, in turn.
        currents: Mutex<Vec<u64>>,
    }

    impl Manual {
        fn tell(&self, step: Step) {
            let _ = self.steps.send(step);
            self.go.lock().unwrap().recv().expect("the test is gone");
        }
    }

    impl Clock for Manual {
        fn now(&self) -> Instant {
            self.zero + *self.at.lock().unwrap()
        }

        fn sleep<'a>(
            &self,
            live: &'a Live,
            plan: MutexGuard<'a, Plan>,
            wait: Option<Duration>,
        ) -> MutexGuard<'a, Plan> {
            drop(plan);
            self.tell(Step::Waits(wait));
            live.plan.lock().unwrap()
        }
    }

    struct Rebuilds {
        clock: Arc<Manual>,
        steps: Receiver<Step>,
        go: Sender<()>,
        thread: JoinHandle<()>,
    }

    fn live(gap: Duration) -> Arc<Live> {
        Arc::new(Live {
            gap,
            ..Live::default()
        })
    }

    /// Starts a rebuild thread whose last rebuild began `ago`.
    fn rebuilds(live: &Arc<Live>, ago: Duration) -> Rebuilds {
        let (tell, steps) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        let clock = Arc::new(Manual {
            // Later than now, so a last rebuild an hour back is still a valid Instant.
            zero: Instant::now() + Duration::from_secs(3600),
            at: Mutex::default(),
            steps: tell,
            go: Mutex::new(wait),
            currents: Mutex::default(),
        });
        let thread = {
            let (live, clock) = (Arc::clone(live), Arc::clone(&clock));
            let last = clock.zero - ago;
            thread::spawn(move || {
                rebuild_loop(&live, &*clock, last, |pass| {
                    let at = clock.now() - clock.zero;
                    clock.currents.lock().unwrap().push(pass.current);
                    clock.tell(Step::Starts {
                        at,
                        full_disk: pass.full_disk,
                        mark: pass.mark,
                        clutter: pass.clutter,
                    });
                });
            })
        };
        let rebuilds = Rebuilds {
            clock,
            steps,
            go,
            thread,
        };
        assert_eq!(rebuilds.next(), Step::Waits(None));
        rebuilds
    }

    impl Rebuilds {
        fn next(&self) -> Step {
            let step = self.steps.recv_timeout(Duration::from_secs(60));
            step.expect("the rebuild thread went quiet")
        }

        /// Lets the rebuild thread go on: it ends a rebuild under way or looks at the plan
        /// again, and says what it does next.
        fn step(&self) -> Step {
            self.go.send(()).unwrap();
            self.next()
        }

        fn set_ms(&self, ms: u64) {
            *self.clock.at.lock().unwrap() = Duration::from_millis(ms);
        }

        fn stop(self, live: &Live) {
            live.plan(|plan| plan.stopped = true);
            self.go.send(()).unwrap();
            // A thread that missed the stop now fails at its next step instead of hanging.
            drop(self.go);
            let stopped = self.thread.join();
            assert!(stopped.is_ok(), "the rebuild thread went on after the stop");
        }
    }

    const LONG_AGO: Duration = Duration::from_secs(600);

    #[test]
    fn a_lone_change_rebuilds_at_once() {
        // A gap of a minute, which a lone change long after the last rebuild doesn't wait for.
        let gap = Duration::from_secs(60);
        let live = live(gap);
        let rebuilds = rebuilds(&live, gap);
        live.plan(|plan| plan.want(4));
        assert_eq!(rebuilds.step(), starts(0, 4));
        assert_eq!(rebuilds.step(), Step::Waits(None));

        // The next one, straight after, waits out the gap.
        live.plan(|plan| plan.want(6));
        assert_eq!(rebuilds.step(), Step::Waits(Some(gap)));
        rebuilds.set_ms(59_999);
        let left = Duration::from_millis(1);
        assert_eq!(rebuilds.step(), Step::Waits(Some(left)));
        rebuilds.set_ms(60_000);
        assert_eq!(rebuilds.step(), starts(60_000, 6));
        rebuilds.stop(&live);
    }

    /// Plays changes at the given milliseconds against a rebuild thread whose first list
    /// started `ago` ms before 0, each rebuild taking `takes` ms, and returns when each rebuild
    /// started and its mark. The change at `changes[i]` has mark `i + 1`. As with a real wait,
    /// the thread looks again only when a change comes, its rebuild ends, or its wait is up.
    fn burst(changes: &[u64], gap: u64, takes: u64, ago: u64, until: u64) -> Vec<(u64, u64)> {
        let live = live(Duration::from_millis(gap));
        let rebuilds = rebuilds(&live, Duration::from_millis(ago));
        // `last` counts from the first list's start, like `expected`.
        let (mut started, mut last, mut wakes) = (Vec::new(), 0, None);
        let mut step = Step::Waits(None);
        for ms in 0..=until {
            rebuilds.set_ms(ms);
            let mut came = false;
            for (mark, _) in (1..).zip(changes).filter(|&(_, &at)| at == ms) {
                live.plan(|plan| plan.want(mark));
                came = true;
            }
            let running = matches!(step, Step::Starts { .. });
            if wakes != Some(ms) && (!came || running) {
                continue;
            }
            step = rebuilds.step();
            wakes = match step {
                Step::Starts { mark, .. } => {
                    started.push((ms, mark));
                    last = ms + ago;
                    Some(ms + takes)
                }
                Step::Waits(Some(left)) => {
                    let gap_left = last + gap - (ms + ago);
                    assert_eq!(left, Duration::from_millis(gap_left), "at {ms} ms");
                    Some(ms + gap_left)
                }
                Step::Waits(None) => None,
            };
        }
        assert_eq!(step, Step::Waits(None), "still busy at {until} ms");
        rebuilds.stop(&live);
        started
    }

    /// The rule written out plainly: a rebuild starts at the first millisecond with a change
    /// waiting, none running, and a gap since the last one began.
    fn expected(changes: &[u64], gap: u64, takes: u64, ago: u64, until: u64) -> Vec<(u64, u64)> {
        // `last` counts from the first list's start.
        let (mut started, mut last, mut free_at, mut next) = (Vec::new(), 0, 0, 0);
        for ms in 0..=until {
            let waiting = changes.get(next).is_some_and(|&at| at <= ms);
            if waiting && ms >= free_at && ms + ago >= last + gap {
                started.push((ms, next as u64 + 1));
                (last, free_at) = (ms + ago, ms + takes);
                next = changes.partition_point(|&at| at <= ms);
            }
        }
        started
    }

    #[test]
    fn a_burst_of_changes_rebuilds_a_gap_apart_and_ends_with_the_last_change() {
        // A save every 7 ms for a third of a second, a 100 ms gap, rebuilds of 30 ms, and the
        // first list a minute back.
        let changes: Vec<u64> = (0..50).map(|i| 7 * i + 1).collect();
        let started = burst(&changes, 100, 30, 60_000, 600);
        // One at once, then one per gap, each reading every change since the one before; the
        // last, at 401 ms, reads the last change, made at 344 ms.
        assert_eq!(started, [(1, 1), (101, 2), (201, 16), (301, 30), (401, 44)]);
        assert_eq!(started, expected(&changes, 100, 30, 60_000, 600));
    }

    #[test]
    fn any_burst_rebuilds_as_the_rule_says() {
        // xorshift64, so every run plays the same bursts.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        for _ in 0..64 {
            // The first list sometimes still inside its gap at 0.
            let (gap, takes, ago) = (random(120), 1 + random(60), random(240));
            let mut at = 0;
            let changes: Vec<u64> = (0..=random(40))
                .map(|_| {
                    // Two changes in the same millisecond now and then, and now and then a
                    // pause longer than any gap.
                    at += match random(8) {
                        0 => 200 + random(400),
                        _ => random(16),
                    };
                    at
                })
                .collect();
            let until = at + 2 * (gap + takes) + 1;
            let case = format!("gap {gap}, takes {takes}, ago {ago}, changes {changes:?}");
            let started = burst(&changes, gap, takes, ago, until);
            assert_eq!(
                started,
                expected(&changes, gap, takes, ago, until),
                "{case}"
            );
            let last = started.last().unwrap();
            assert!(last.0 >= *changes.last().unwrap(), "{case}");
        }
    }

    #[test]
    fn changes_during_a_rebuild_wait_as_one_more() {
        let live = live(Duration::ZERO);
        let rebuilds = rebuilds(&live, LONG_AGO);
        live.plan(|plan| plan.want(1));
        assert_eq!(rebuilds.step(), starts(0, 1));
        for mark in 2..22 {
            live.plan(|plan| plan.want(mark));
        }
        assert_eq!(rebuilds.step(), starts(0, 2));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_change_during_a_rebuild_is_judged_by_the_list_it_brings() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let notes = home.join("notes");
        let live = live(Duration::ZERO);
        // What a rebuild reading the folder now would hand on.
        let list = || {
            let settings =
                Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
            let index = settings.scan().unwrap().index;
            let scope = Some(settings.scope(&index));
            Built { index, scope }
        };
        live.publish(list(), 0);
        let core = idle_core(&live);
        // Unless the test hands one on, each rebuild leaves the list as it was.
        let rebuilds = rebuilds(&live, LONG_AGO);

        live.plan(|plan| plan.want(1));
        assert_eq!(rebuilds.step(), starts(0, 1));
        std::fs::write(notes.join("budget.txt"), "edited").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 2), 1);
        assert!(!live.plan.lock().unwrap().wanted);
        assert_eq!(rebuilds.step(), Step::Waits(None));

        // The list it brings has the new file, though the one before didn't.
        live.plan(|plan| plan.want(3));
        assert_eq!(rebuilds.step(), starts(0, 3));
        std::fs::write(notes.join("plan.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 4), 1);
        live.publish(list(), 3);
        assert_eq!(rebuilds.step(), Step::Waits(None));

        live.plan(|plan| plan.want(5));
        assert_eq!(rebuilds.step(), starts(0, 5));
        std::fs::write(notes.join("todo.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&notes, &notes], 0, 7), 1);
        assert_eq!(changed_at(&core, &[&notes], 0, 6), 1);
        assert_eq!(
            live.plan.lock().unwrap().recheck,
            Some((vec![notes.clone()], 6))
        );
        assert_eq!(live.plan.lock().unwrap().delivered, 7);
        // Read again alone, not built again.
        assert_eq!(rebuilds.step(), rereads(0, 6));
        assert_eq!(live.plan.lock().unwrap().changes, Vec::<PathBuf>::new());
        rebuilds.stop(&live);
    }

    #[test]
    fn a_list_is_current_to_the_changes_heard_before_its_rebuild_started() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        live.plan(|plan| {
            plan.heard(7);
            plan.want(7);
        });
        assert_eq!(rebuilds.step(), starts(0, 7));
        // Heard while that one runs, so only the next one reads it.
        assert_eq!(changed_at(&core, &[], 1, 9), 1);
        assert_eq!(rebuilds.step(), starts(0, 9));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        assert_eq!(*rebuilds.clock.currents.lock().unwrap(), [7, 9]);
        rebuilds.stop(&live);
    }

    #[test]
    fn an_ignore_list_edited_during_a_rebuild_brings_a_whole_one() {
        let (_dir, home) = home_with(&["notes/a.txt"]);
        let rules = home.join(".config/argus");
        let live = live(Duration::ZERO);
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let index = settings.scan().unwrap().index;
        let scope = Some(settings.scope(&index));
        live.publish(Built { index, scope }, 0);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        live.plan(|plan| plan.want(1));
        assert_eq!(rebuilds.step(), starts(0, 1));
        assert_eq!(changed_at(&core, &[&rules], 0, 2), 1);
        assert_eq!(
            live.plan.lock().unwrap().recheck,
            Some((vec![rules.clone()], 2))
        );
        // By the new rules, not the folder read again.
        assert_eq!(rebuilds.step(), starts(0, 2));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_plan_is_busy_only_while_a_list_is_on_its_way() {
        assert!(!Plan::default().busy());
        let busy: [fn(&mut Plan); 4] = [
            |plan| plan.want(1),
            |plan| plan.running = true,
            |plan| plan.patch(1),
            |plan| plan.everything = true,
        ];
        for (at, make) in busy.iter().enumerate() {
            let mut plan = Plan::default();
            make(&mut plan);
            assert!(plan.busy(), "{at}");
        }
        let failed = Plan {
            failed: Some(1),
            ..Plan::default()
        };
        assert!(
            !failed.busy() && !failed.idle(),
            "a failed rebuild waits for the next change; tl_health tells it apart"
        );
        assert!(
            !Plan::new(true, false).busy(),
            "keeping everything from the start"
        );
        let listed = Plan {
            running: true,
            listed: true,
            ..Plan::default()
        };
        assert!(!listed.busy(), "its list is in use");
        assert!(!listed.idle(), "late changes are still being checked");
    }

    #[test]
    fn only_a_list_read_from_the_whole_folder_counts_as_building() {
        assert!(!Plan::default().building());
        let building: [fn(&mut Plan); 3] = [
            |plan| plan.want(1),
            |plan| (plan.running, plan.whole) = (true, true),
            |plan| plan.everything = true,
        ];
        for (at, make) in building.iter().enumerate() {
            let mut plan = Plan::default();
            make(&mut plan);
            assert!(plan.building() && plan.busy(), "{at}");
        }
        let patches: [fn(&mut Plan); 2] = [|plan| plan.patch(1), |plan| plan.running = true];
        for (at, make) in patches.iter().enumerate() {
            let mut plan = Plan::default();
            make(&mut plan);
            assert!(plan.busy() && !plan.building(), "{at}");
        }
        let listed = Plan {
            running: true,
            whole: true,
            listed: true,
            ..Plan::default()
        };
        assert!(!listed.building(), "its list is in use");
    }

    #[test]
    fn health_tells_a_patch_from_a_whole_new_list() {
        let live = live(Duration::from_secs(3600));
        let core = idle_core(&live);
        live.plan(|plan| plan.patch(1));
        let patching = health(&core);
        assert!(patching.busy && !patching.building, "{patching:?}");
        live.plan(|plan| plan.want(2));
        let building = health(&core);
        assert!(building.busy && building.building, "{building:?}");
        live.plan(|plan| plan.stopped = true);
        let stopped = health(&core);
        assert!(!stopped.busy && !stopped.building, "{stopped:?}");
    }

    #[test]
    fn only_a_plan_with_nothing_waiting_or_running_is_idle() {
        assert!(Plan::default().idle());
        let busy: [fn(&mut Plan); 6] = [
            |plan| plan.want(1),
            |plan| plan.running = true,
            |plan| plan.patch(1),
            |plan| plan.everything = true,
            |plan| plan.recheck = Some((Vec::new(), 1)),
            |plan| plan.failed = Some(1),
        ];
        for (at, make) in busy.iter().enumerate() {
            let mut plan = Plan::default();
            make(&mut plan);
            assert!(!plan.idle(), "{at}");
        }
    }

    #[test]
    fn full_disk_access_reaches_one_rebuild() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        let told = |mark| Step::Starts {
            at: Duration::ZERO,
            full_disk: true,
            mark,
            clutter: Some(Clutter::Skip),
        };
        live.plan(|plan| {
            plan.full_disk = true;
            plan.wanted = true;
        });
        assert_eq!(rebuilds.step(), told(0));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        live.plan(|plan| plan.wanted = true);
        assert_eq!(rebuilds.step(), starts(0, 0));

        // Granted while that one runs, so it reaches the one after.
        unsafe { tl_grant_full_disk(core.0, 0, 5) };
        assert_eq!(rebuilds.step(), told(5));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        live.plan(|plan| plan.want(6));
        assert_eq!(rebuilds.step(), starts(0, 6));
        rebuilds.stop(&live);
    }

    /// The window turns the switch on as access comes, kept from the last launch, and the
    /// rebuild thread may wake between two calls: one list must bring both.
    #[test]
    fn full_disk_access_with_the_switch_on_reaches_one_list() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        unsafe { tl_grant_full_disk(core.0, 1, 5) };
        let both = Step::Starts {
            at: Duration::ZERO,
            full_disk: true,
            mark: 5,
            clutter: Some(Clutter::Keep),
        };
        assert_eq!(rebuilds.step(), both);
        assert_eq!(rebuilds.step(), Step::Waits(None), "no second list");
        assert_eq!(switch(&core, 1, 6), 0, "already on");
        rebuilds.stop(&live);
    }

    /// A core with no threads of its own, whose plan the test drives.
    fn idle_core(live: &Arc<Live>) -> Core {
        Core(Box::into_raw(Box::new(TlCore {
            searcher: Arc::default(),
            jobs: None,
            done: Arc::default(),
            live: Arc::clone(live),
            worker: None,
            saver: None,
            saved_in: None,
            text: None,
            judge: None,
        })))
    }

    fn switch(core: &Core, on: u32, mark: u64) -> u32 {
        unsafe { tl_index_everything(core.0, on, mark) }
    }

    fn keeps_everything(mark: u64) -> Step {
        Step::Starts {
            at: Duration::ZERO,
            full_disk: false,
            mark,
            clutter: Some(Clutter::Keep),
        }
    }

    #[test]
    fn a_volume_change_alone_keeps_the_root_and_waits_out_the_gap() {
        let gap = Duration::from_millis(200);
        let live = live(gap);
        let rebuilds = rebuilds(&live, Duration::ZERO);
        live.plan(|plan| plan.patch(5));
        live.plan(|plan| plan.patch(4));
        assert_eq!(rebuilds.step(), Step::Waits(Some(gap)));
        rebuilds.set_ms(200);
        let volumes_only = Step::Starts {
            at: gap,
            full_disk: false,
            mark: 4,
            clutter: None,
        };
        assert_eq!(rebuilds.step(), volumes_only);
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_volume_change_beside_a_whole_rebuild_rides_along() {
        let live = live(Duration::from_millis(200));
        let rebuilds = rebuilds(&live, LONG_AGO);
        live.plan(|plan| {
            plan.want(8);
            plan.granted = true;
            plan.volumes.push("/Volumes/Photos".into());
            plan.changes.push("/Volumes/Photos/2026".into());
            plan.patch(6);
        });
        assert_eq!(rebuilds.step(), starts(0, 6));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        {
            let plan = live.plan.lock().unwrap();
            assert!(plan.changes.is_empty() && !plan.patching);
            assert_eq!(plan.patched, None);
        }
        rebuilds.stop(&live);
    }

    #[test]
    fn switching_back_before_the_rebuild_starts_asks_for_none() {
        let gap = Duration::from_millis(300);
        let live = live(gap);
        let core = idle_core(&live);
        // A list just started, so the next waits out the gap.
        let rebuilds = rebuilds(&live, Duration::ZERO);
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(rebuilds.step(), Step::Waits(Some(gap)));
        assert_eq!(switch(&core, 1, 4), 0, "already on");
        assert_eq!(switch(&core, 0, 5), 0);
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.set_ms(600);
        assert_eq!(rebuilds.step(), Step::Waits(None));
        live.plan(|plan| plan.want(7));
        assert_eq!(rebuilds.step(), starts(600, 7));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_switch_while_a_rebuild_runs_waits_as_one_more() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(rebuilds.step(), keeps_everything(3));
        // Off, on and off again while it runs: only the last differs from what it builds.
        assert_eq!(switch(&core, 0, 4), 1);
        assert_eq!(switch(&core, 1, 5), 0);
        assert_eq!(switch(&core, 0, 6), 1);
        assert_eq!(rebuilds.step(), starts(0, 6));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_change_and_a_switch_both_waiting_start_one_rebuild_from_the_first() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let rebuilds = rebuilds(&live, LONG_AGO);
        assert_eq!(switch(&core, 1, 3), 1);
        live.plan(|plan| plan.want(5));
        assert_eq!(rebuilds.step(), keeps_everything(3));
        assert_eq!(rebuilds.step(), Step::Waits(None));

        // Switched off and back before it starts, so only the change counts.
        assert_eq!(switch(&core, 0, 6), 1);
        live.plan(|plan| plan.want(8));
        assert_eq!(switch(&core, 1, 9), 0);
        assert_eq!(rebuilds.step(), keeps_everything(8));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    #[test]
    fn a_change_while_a_rebuild_keeping_everything_runs_waits_for_its_list() {
        let (_dir, home) = home_with(&["notes/a.txt", "Library/Caches/com.example/cache.db"]);
        let caches = home.join("Library/Caches/com.example");
        let list = |clutter| {
            let settings = Settings::new(&home, &home, clutter, Ignore::default(), Access::Full);
            let index = settings.scan().unwrap().index;
            let scope = Some(settings.scope(&index));
            Built { index, scope }
        };
        let live = live(Duration::ZERO);
        live.publish(list(Clutter::Skip), 0);
        let core = idle_core(&live);
        assert_eq!(changed(&core, &[&caches], 0), 0);
        let rebuilds = rebuilds(&live, LONG_AGO);
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(rebuilds.step(), keeps_everything(3));
        // The rebuild read the caches just before the new file came.
        let read = list(Clutter::Keep);
        std::fs::write(caches.join("new.db"), "").unwrap();
        assert_eq!(changed_at(&core, &[&caches], 0, 4), 1);
        assert_eq!(
            live.plan.lock().unwrap().recheck,
            Some((vec![caches.clone()], 4))
        );
        live.publish(read, 3);
        assert_eq!(rebuilds.step(), rereads(0, 4));
        assert_eq!(rebuilds.step(), Step::Waits(None));
        rebuilds.stop(&live);
    }

    /// The wall clock, telling the test of each wait before it begins.
    struct Told(Sender<Option<Duration>>);

    impl Clock for Told {
        fn now(&self) -> Instant {
            Wall.now()
        }

        fn sleep<'a>(
            &self,
            live: &'a Live,
            plan: MutexGuard<'a, Plan>,
            wait: Option<Duration>,
        ) -> MutexGuard<'a, Plan> {
            let _ = self.0.send(wait);
            Wall.sleep(live, plan, wait)
        }
    }

    #[test]
    fn stopping_wakes_a_rebuild_waiting_out_its_gap() {
        let hour = Duration::from_secs(3600);
        let live = live(hour);
        live.plan(|plan| plan.wanted = true);
        let (tell, waits) = mpsc::channel();
        let (done, stopped) = mpsc::channel();
        {
            let live = Arc::clone(&live);
            thread::spawn(move || {
                rebuild_loop(&live, &Told(tell), Instant::now(), |_| {
                    panic!("no rebuild within the hour");
                });
                let _ = done.send(());
            });
        }
        let wait = waits.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(wait.is_some_and(|wait| wait > hour / 2), "{wait:?}");
        // Told while it still holds the plan, so this waits until it is asleep.
        live.plan(|plan| plan.stopped = true);
        let woke = stopped.recv_timeout(Duration::from_secs(60));
        assert!(woke.is_ok(), "stopping didn't cut the hour short");
    }

    /// The hand-moved clock steps the thread whether or not anything woke it, so this one runs
    /// on the wall clock.
    #[test]
    fn each_way_of_asking_wakes_a_sleeping_rebuild_thread() {
        let live = live(Duration::ZERO);
        let core = idle_core(&live);
        let (tell, waits) = mpsc::channel();
        let (began, starts) = mpsc::channel();
        {
            let live = Arc::clone(&live);
            thread::spawn(move || {
                rebuild_loop(&live, &Told(tell), Instant::now(), |pass| {
                    let _ = began.send((pass.full_disk, pass.clutter, pass.mark));
                });
            });
        }
        let minute = Duration::from_secs(60);
        // Told while it still holds the plan, so each ask comes once it is asleep.
        let asleep = || assert_eq!(waits.recv_timeout(minute), Ok(None));
        let next_start = || starts.recv_timeout(minute);
        asleep();
        assert_eq!(changed_at(&core, &[], 1, 1), 1);
        assert_eq!(next_start(), Ok((false, Some(Clutter::Skip), 1)));
        asleep();
        unsafe { tl_grant_full_disk(core.0, 0, 2) };
        assert_eq!(next_start(), Ok((true, Some(Clutter::Skip), 2)));
        asleep();
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(next_start(), Ok((false, Some(Clutter::Keep), 3)));
        asleep();
        live.plan(|plan| plan.patch(4));
        assert_eq!(next_start(), Ok((false, None, 4)));
    }

    #[test]
    fn a_panic_becomes_an_error_code() {
        assert_eq!(
            guard(TL_PANICKED, || panic!("a bug in the engine")),
            TL_PANICKED
        );
    }

    /// Run with `cargo test --release -p argus-ffi -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds a million names; slow outside --release"]
    fn eight_fast_keystrokes_show_two_results_or_fewer() {
        let events = Events::default();
        let core = start(1_000_000, &events);
        for query in ["argus plan", "budget notes", "readme", "会議 report"] {
            let typed: Vec<&str> = query
                .char_indices()
                .map(|(i, c)| &query[..i + c.len_utf8()])
                .collect();
            let before = events.ready().len();
            let started = Instant::now();
            let tickets: Vec<u64> = (0..8)
                .map(|i| submit(&core, typed[i.min(typed.len() - 1)]))
                .collect();
            let last = tickets[7];
            events.wait_for(TL_EVENT_READY, last);
            let shown = events.ready().len() - before;
            println!(
                "{query:?}: {shown} shown, last after {:?}",
                started.elapsed()
            );
            assert!(shown <= 2);
            assert!(take(&core, last).is_some());
        }
    }

    /// Every constant, function signature and struct field, written in Rust's spelling on
    /// both sides, so a mismatch shows up here instead of as garbage in Swift.
    #[test]
    fn the_header_matches_the_rust_side() {
        // Git on Windows may check the files out with \r\n line endings.
        let source = include_str!("lib.rs").replace('\r', "");
        let rust = source.split("#[cfg(test)]").next().unwrap();
        let header = &include_str!("../include/argus.h").replace('\r', "");

        let consts = |source: &str, prefix: &str| -> Vec<String> {
            source
                .lines()
                .filter_map(|line| line.strip_prefix(prefix))
                .map(|rest| rest.replace(": u32", "").replace(' ', ""))
                .collect()
        };
        assert_eq!(
            consts(rust, "pub const TL_"),
            consts(header, "static const uint32_t TL_")
        );

        let rust_functions: Vec<String> = rust
            .split("extern \"C\" fn tl_")
            .skip(1)
            .map(|rest| {
                let (name, rest) = rest.split_once('(').unwrap();
                let (params, rest) = rest.split_once(')').unwrap();
                let returns = rest.split('{').next().unwrap().trim();
                let returns = returns.trim_start_matches("->").trim();
                format!("tl_{name}({}) {returns}", rust_params(params))
            })
            .collect();
        let header_functions: Vec<String> = header
            .lines()
            .filter(|line| line.ends_with(");") && line.contains("tl_"))
            .map(|line| {
                let (head, params) = line.split_once('(').unwrap();
                let (returns, name) = split_c(head);
                let returns = match returns.trim() {
                    "void" => String::new(),
                    returns => rust_type(returns),
                };
                format!(
                    "{name}({}) {returns}",
                    c_params(params.trim_end_matches(");"))
                )
            })
            .collect();
        assert_eq!(rust_functions, header_functions);

        assert_eq!(
            rust.matches("#[unsafe(no_mangle)]").count(),
            rust_functions.len()
        );

        // The callback returns nothing on both sides: `);` straight after its parameters.
        let rust_event = rust
            .split_once("pub type TlEventFn = unsafe extern \"C\" fn(")
            .and_then(|(_, rest)| rest.split_once(");"))
            .unwrap()
            .0;
        assert!(!rust_event.contains(')'));
        let header_event = header
            .split_once("typedef void (*TlEventFn)(")
            .and_then(|(_, rest)| rest.split_once(");"))
            .unwrap()
            .0;
        assert_eq!(rust_params(rust_event), c_params(header_event));

        for name in [
            "TlRow", "TlRows", "TlSaved", "TlLine", "TlReport", "TlLines", "TlHealth",
        ] {
            assert!(rust.contains(&format!("#[repr(C)]\npub struct {name} {{")));
            let rust_fields: Vec<String> = block(rust, &format!("pub struct {name} {{"))
                .map(|line| {
                    let (field, ty) = line.trim_start_matches("pub ").split_once(':').unwrap();
                    format!("{field}: {}", ty.trim().trim_end_matches(','))
                })
                .collect();
            let header_fields: Vec<String> = block(header, &format!("typedef struct {name} {{"))
                .map(|line| {
                    let (ty, field) = split_c(line.trim_end_matches(';'));
                    format!("{field}: {}", rust_type(ty))
                })
                .collect();
            assert_eq!(rust_fields, header_fields, "fields of {name}");
        }
    }

    /// The trimmed lines between `opening` and the closing brace.
    fn block<'a>(source: &'a str, opening: &str) -> impl Iterator<Item = &'a str> {
        source
            .lines()
            .skip_while(move |line| *line != opening)
            .skip(1)
            .take_while(|line| !line.starts_with('}'))
            .map(str::trim)
    }

    /// "ctx: *mut c_void, event: u32" becomes "*mut c_void, u32".
    fn rust_params(list: &str) -> String {
        let types: Vec<&str> = list
            .split(',')
            .filter_map(|param| param.split_once(':'))
            .map(|(_, ty)| ty.trim())
            .collect();
        types.join(", ")
    }

    /// "void *ctx, uint32_t event" becomes "*mut c_void, u32".
    fn c_params(list: &str) -> String {
        let types: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|param| *param != "void")
            .map(|param| rust_type(split_c(param).0))
            .collect();
        types.join(", ")
    }

    /// "const TlRow *rows" becomes ("const TlRow *", "rows").
    fn split_c(declaration: &str) -> (&str, &str) {
        let at = declaration.rfind([' ', '*']).unwrap() + 1;
        (&declaration[..at], &declaration[at..])
    }

    /// A C type in Rust's spelling. A C function pointer may be null, so it is an `Option`.
    fn rust_type(c: &str) -> String {
        let c = c.trim();
        if let Some(target) = c.strip_suffix('*') {
            return match target.trim().strip_prefix("const ") {
                Some(target) => format!("*const {}", rust_type(target)),
                None => format!("*mut {}", rust_type(target)),
            };
        }
        match c {
            "void" => "c_void",
            "uint8_t" => "u8",
            "uint32_t" => "u32",
            "uint64_t" => "u64",
            "size_t" => "usize",
            "TlEventFn" => "Option<TlEventFn>",
            other => other,
        }
        .to_owned()
    }
}
