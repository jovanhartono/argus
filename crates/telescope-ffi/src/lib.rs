//! The hatch: the few C-style functions a window calls to reach the engine. The macOS
//! window calls them from Swift, and the Windows window will reuse them at M5.
//!
//! The window calls in from its main thread and never waits for a search. `tl_submit` hands
//! the search to a worker and returns its ticket at once; the worker calls `on_event` when
//! that ticket's rows are ready; the window then copies them out with `tl_take`.
//! When files change, `tl_changed` asks a second thread to build the list again; the new list
//! reaches the worker through the same queue as the searches, so each search sees one whole list.
//! A text search, inside files, runs the same way on a worker of its own, started with the
//! first one, so a name search never waits behind it: `tl_grep_submit`, then `tl_grep_take`
//! for each batch of lines found.
//! `include/telescope.h` repeats everything here for Swift, and a test keeps the two in step.

use std::ffi::{CString, c_void};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use telescope_core::{
    Access, Apps, Clutter, Hit, Ignore, Index, Pages, Scope, Searcher, Settings, Ticket, Watch,
};
use telescope_grep::{Found, Grep, Report};

#[global_allocator]
static PAGES: Pages = Pages;

/// Raise this with any change below, so a window built against an older header refuses to start.
pub const TL_ABI_VERSION: u32 = 5;

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
/// A problem with your ignore list. `value` is the address of its text, NUL-terminated UTF-8,
/// readable only until `on_event` returns.
pub const TL_EVENT_NOTE: u32 = 4;
/// A list built again after a change is in use. `value` holds its entry count in the low 32
/// bits and the milliseconds the rebuild took in the high 32.
pub const TL_EVENT_REBUILT: u32 = 5;
/// `value` is the text search ticket whose lines, or whose end, wait for `tl_grep_take`. One
/// per take: no more come until the window has taken what was waiting.
pub const TL_EVENT_LINES: u32 = 6;

const ROWS: usize = 50;
/// Lines a text search shows, across all its folders.
const LINES: usize = 100;

/// Rebuilds start at least this far apart while files keep changing, so a long download
/// doesn't keep a core busy. A change is findable within FSEvents' 0.2 s, plus this, plus
/// one rebuild: about 0.15 s for a home folder.
const GAP: Duration = Duration::from_millis(500);

/// Runs on Rust threads, at times several at once, never the window's main thread, and must
/// never wait for it.
pub type TlEventFn = unsafe extern "C" fn(ctx: *mut c_void, event: u32, value: u64);

/// One result. Its name and folder are byte ranges in `TlRows::text`.
#[repr(C)]
pub struct TlRow {
    pub name_start: u32,
    pub name_len: u32,
    pub folder_start: u32,
    pub folder_len: u32,
    pub is_folder: bool,
}

/// The window copies what it needs, then hands this back to `tl_rows_free` once.
#[repr(C)]
pub struct TlRows {
    pub rows: *const TlRow,
    pub count: usize,
    pub text: *const u8,
    owner: *mut c_void,
}

struct Packed {
    rows: Vec<TlRow>,
    text: String,
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

/// What a text search did with each file it met, as `telescope_grep::Report` explains.
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
    /// None only for a core a test built without threads.
    text: Option<Text>,
}

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
    },
    /// A list built again, and the milliseconds that took.
    Swap {
        index: Arc<Index>,
        took: u32,
    },
}

/// What the rebuild thread waits on.
#[derive(Default)]
struct Plan {
    /// Something changed since the last rebuild started.
    wanted: bool,
    /// A rebuild is under way, so the newest list may already be out of date.
    running: bool,
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
}

impl Plan {
    fn new(everything: bool) -> Self {
        Self {
            everything,
            everything_listed: everything,
            ..Self::default()
        }
    }

    fn want(&mut self, mark: u64) {
        self.wanted = true;
        self.asked = Some(self.asked.map_or(mark, |asked| asked.min(mark)));
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
}

/// The newest list, shared with the worker rather than copied, and the rules it was built by.
type Newest = Option<(Arc<Scope>, Arc<Index>)>;

/// The folders to watch for changes to the newest list, and the mark of the first change its
/// rebuild was asked for, or 0 for the first list.
type Watched = (Vec<(String, Watch)>, u64);

#[derive(Default)]
struct Live {
    plan: Mutex<Plan>,
    wake: Condvar,
    /// None for made-up names, or before the first list.
    newest: Mutex<Newest>,
    watched: Mutex<Watched>,
}

impl Live {
    fn plan(&self, change: impl FnOnce(&mut Plan)) {
        change(&mut self.plan.lock().unwrap());
        self.wake.notify_all();
    }

    /// Keeps the list and its rules, for judging later changes, and hands the list on.
    fn publish(&self, built: Built, mark: u64) -> Arc<Index> {
        let index = Arc::new(built.index);
        let watched = built
            .scope
            .as_ref()
            .map(|scope| scope.watched(&index))
            .unwrap_or_default();
        let watched = watched
            .into_iter()
            .map(|(path, watch)| (path.to_string_lossy().into_owned(), watch))
            .collect();
        *self.watched.lock().unwrap() = (watched, mark);
        *self.newest.lock().unwrap() = built
            .scope
            .map(|scope| (Arc::new(scope), Arc::clone(&index)));
        index
    }

    fn misses_a_change_in(&self, paths: &[PathBuf]) -> bool {
        let newest = self.newest.lock().unwrap().clone();
        newest.is_some_and(|(scope, index)| paths.iter().any(|path| !scope.holds(path, &index)))
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
    },
}

struct Built {
    index: Index,
    scope: Option<Scope>,
}

impl Source {
    fn build(&mut self, clutter: Clutter, notify: &Notify) -> io::Result<Built> {
        let (root, access, told) = match self {
            Self::StandIn(count) => {
                let index = telescope_core::stand_in(*count);
                return Ok(Built { index, scope: None });
            }
            Self::Home { root, access, told } => (root, *access, told),
        };
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
        let index = settings.scan()?.index;
        Ok(Built {
            index,
            scope: Some(settings.scope()),
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
/// `TL_EVENT_NOTE` first. Returns null on failure, without `on_event`, or for a `root` that
/// isn't UTF-8.
///
/// # Safety
/// `root` points to `root_len` bytes. `on_event` may be called with `ctx` from any thread
/// until `tl_core_free` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_core_new(
    root: *const u8,
    root_len: usize,
    full_disk: u32,
    everything: u32,
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
) -> *mut TlCore {
    guard(std::ptr::null_mut(), || {
        if root.is_null() {
            return std::ptr::null_mut();
        }
        let bytes = unsafe { std::slice::from_raw_parts(root, root_len) };
        let Ok(root) = std::str::from_utf8(bytes).map(PathBuf::from) else {
            return std::ptr::null_mut();
        };
        let access = match full_disk {
            0 => Access::Limited,
            _ => Access::Full,
        };
        let told = Vec::new();
        let source = Source::Home { root, access, told };
        start(ctx, on_event, source, everything != 0)
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
        start(ctx, on_event, Source::StandIn(count as usize), false)
    })
}

fn start(
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
    source: Source,
    everything: bool,
) -> *mut TlCore {
    let Some(on_event) = on_event else {
        return std::ptr::null_mut();
    };
    let searcher = Arc::new(Searcher::default());
    let done = Arc::default();
    let live = Arc::new(Live {
        plan: Mutex::new(Plan::new(everything)),
        ..Live::default()
    });
    let (jobs, inbox) = mpsc::channel();
    let notify = Notify { ctx, on_event };
    let (home, full_disk) = match &source {
        // Real folders all the same, so no pop-ups before `tl_grant_full_disk`.
        Source::StandIn(_) => (telescope_core::home(), false),
        Source::Home { root, access, .. } => (root.clone(), *access == Access::Full),
    };
    let text = Text {
        home,
        full_disk: AtomicBool::new(full_disk),
        notify,
        started: OnceLock::new(),
    };
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
        text: Some(text),
    }))
}

/// Builds the first list, then answers searches. The rebuild thread it starts holds `swaps`,
/// so the queue closes only once both the core and that thread are gone.
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
    let first = panic::catch_unwind(AssertUnwindSafe(|| {
        let clutter = live.plan.lock().unwrap().list();
        source
            .build(clutter, &notify)
            .map(|built| live.publish(built, 0))
    }));
    let index = match first {
        Ok(Ok(index)) => index,
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
    notify.send(TL_EVENT_INDEXED, index.entry_count() as u64);
    let rebuilds = {
        let live = Arc::clone(live);
        thread::Builder::new()
            .name("rebuild".into())
            .spawn(move || {
                rebuild_loop(&live, GAP, started, |full_disk, clutter, mark| {
                    rebuild(
                        &mut source,
                        full_disk,
                        clutter,
                        mark,
                        &live,
                        &swaps,
                        &notify,
                    );
                });
            })
            .expect("start the rebuild thread")
    };
    let served = panic::catch_unwind(AssertUnwindSafe(|| {
        serve(index, searcher, inbox, done, &notify);
    }));
    if served.is_err() {
        live.plan(|plan| plan.stopped = true);
        notify.send(TL_EVENT_FAILED, 0);
    }
    // Waited for even after a bug, so it can't call `on_event` once the core is freed.
    let _ = rebuilds.join();
}

fn serve(
    mut index: Arc<Index>,
    searcher: &Searcher,
    inbox: &Receiver<Job>,
    done: &Done,
    notify: &Notify,
) {
    // The window has no app rows yet.
    let apps = Apps::default();
    while let Ok(job) = inbox.recv() {
        let (ticket, query) = match job {
            // Searches sent before this one ran on the old list; the old list is freed here.
            Job::Swap { index: fresh, took } => {
                index = fresh;
                notify.send(
                    TL_EVENT_REBUILT,
                    u64::from(took) << 32 | index.entry_count() as u64,
                );
                continue;
            }
            Job::Search { ticket, query } => (ticket, query),
        };
        // A search replaced by a newer keystroke gives up and is never shown.
        let Some(hits) = searcher.search(&index, &apps, &query, ticket, ROWS) else {
            continue;
        };
        // A key pressed just as the search finished: skip rows the window would refuse.
        if searcher.newest() != ticket.number() {
            continue;
        }
        *done.lock().unwrap() = Some((ticket, pack(&index, &hits)));
        notify.send(TL_EVENT_READY, ticket.number());
    }
}

/// Calls `rebuild` for each rebuild wanted until the core stops: one at a time, with changes
/// made meanwhile waiting as one more, each starting at least `gap` after the one before.
/// `last` is when the first list was started. `rebuild` is told when Full Disk Access came on,
/// whether to keep clutter, and the mark of the first change it reads.
fn rebuild_loop(
    live: &Live,
    gap: Duration,
    mut last: Instant,
    mut rebuild: impl FnMut(bool, Clutter, u64),
) {
    let mut plan = live.plan.lock().unwrap();
    loop {
        if plan.stopped {
            return;
        }
        if !plan.wanted && plan.everything == plan.everything_listed {
            plan = live.wake.wait(plan).unwrap();
            continue;
        }
        let now = Instant::now();
        if now < last + gap {
            plan = live.wake.wait_timeout(plan, last + gap - now).unwrap().0;
            continue;
        }
        plan.wanted = false;
        plan.running = true;
        let full_disk = std::mem::take(&mut plan.full_disk);
        let mark = [plan.asked.take(), plan.switched];
        let mark = mark.into_iter().flatten().min().unwrap_or_default();
        let clutter = plan.list();
        drop(plan);
        last = now;
        rebuild(full_disk, clutter, mark);
        plan = live.plan.lock().unwrap();
        plan.running = false;
        if let Some((paths, mark)) = plan.recheck.take() {
            drop(plan);
            let missed = live.misses_a_change_in(&paths);
            plan = live.plan.lock().unwrap();
            if missed {
                plan.want(mark);
            }
        }
    }
}

/// A failed rebuild keeps the list in use and is told as a note.
fn rebuild(
    source: &mut Source,
    full_disk: bool,
    clutter: Clutter,
    mark: u64,
    live: &Live,
    swaps: &Sender<Job>,
    notify: &Notify,
) {
    if let (true, Source::Home { access, .. }) = (full_disk, &mut *source) {
        *access = Access::Full;
    }
    let started = Instant::now();
    let built = panic::catch_unwind(AssertUnwindSafe(|| {
        source
            .build(clutter, notify)
            .map(|built| live.publish(built, mark))
    }))
    .unwrap_or_else(|_| Err(io::Error::other("a bug in the engine")));
    match built {
        Ok(index) => {
            let took = started.elapsed().as_millis().min(u32::MAX.into()) as u32;
            // Fails only once the worker has stopped.
            let _ = swaps.send(Job::Swap { index, took });
        }
        Err(error) => notify.note(&format!("the list could not be built again: {error}")),
    }
}

fn pack(index: &Index, hits: &[Hit]) -> Packed {
    let mut packed = Packed {
        rows: Vec::with_capacity(hits.len()),
        text: String::new(),
    };
    // An app hit would take a row's place yet show nothing; `serve` passes no apps until
    // TlRow can show one.
    debug_assert!(hits.iter().all(|hit| hit.entry().is_some()), "{hits:?}");
    for entry in hits.iter().filter_map(Hit::entry) {
        let path = index.path(entry);
        let folder = path.parent().unwrap_or(&path).to_string_lossy();
        let name_start = packed.text.len() as u32;
        packed.text.push_str(index.name(entry));
        let folder_start = packed.text.len() as u32;
        packed.text.push_str(&folder);
        packed.rows.push(TlRow {
            name_start,
            name_len: folder_start - name_start,
            folder_start,
            folder_len: folder.len() as u32,
            is_folder: index.is_dir(entry),
        });
    }
    packed
}

/// Starts a search and returns its ticket, or 0 on failure. Every older search gives up.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`, and `query` points to `len` bytes of UTF-8
/// (or `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_submit(core: *const TlCore, query: *const u8, len: usize) -> u64 {
    guard(0, || {
        let core = unsafe { &*core };
        // An empty search box can arrive as a null pointer, which a slice must never hold.
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(query, len) },
        };
        let ticket = core.searcher.ticket();
        let query = String::from_utf8_lossy(bytes).into_owned();
        // Sending fails only once the worker has stopped, which it reported as TL_EVENT_FAILED.
        let sent = core
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job::Search { ticket, query }).is_ok());
        if sent { ticket.number() } else { 0 }
    })
}

/// Tells the core that folders changed. `paths` holds `len` bytes: real paths in UTF-8, each
/// ending in a NUL byte. The list is built again in the background when one is a folder the
/// list reads whose names it no longer matches, or whatever they are when `rescan` isn't 0,
/// as when macOS dropped changes; `TL_EVENT_REBUILT` follows. `mark` is the window's own
/// count of changes so far, handed back by `tl_watched`. Returns 1 when a new list is
/// coming, else 0, as once the core has stopped. Safe to call from any thread.
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
        let (running, everything) = {
            let mut plan = live.plan.lock().unwrap();
            if plan.stopped {
                return 0;
            }
            // A rebuild not started yet will read every change made before it starts.
            if plan.wanted {
                plan.want(mark);
                return 1;
            }
            (plan.running, plan.everything_listed)
        };
        let bytes = match len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(paths, len) },
        };
        let paths: Option<Vec<&Path>> = bytes
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| std::str::from_utf8(path).ok().map(Path::new))
            .collect();
        let newest = live.newest.lock().unwrap().clone();
        let wanted = match (paths, newest) {
            _ if rescan != 0 => true,
            (_, None) => false,
            (None, _) => true,
            (Some(paths), Some((scope, index))) if running => {
                // The rebuild under way may have read these folders before they changed. One
                // keeping everything reads folders the newest list's rules leave out.
                let changed: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|path| everything || scope.in_scope(path) || !scope.holds(path, &index))
                    .map(Path::to_path_buf)
                    .collect();
                if changed.is_empty() {
                    return 0;
                }
                let mut plan = live.plan.lock().unwrap();
                if plan.running {
                    let (waiting, asked) = plan.recheck.get_or_insert_with(|| (Vec::new(), mark));
                    *asked = (*asked).min(mark);
                    for path in changed {
                        if !waiting.contains(&path) {
                            waiting.push(path);
                        }
                    }
                } else {
                    // It ended just now, maybe without reading them.
                    plan.want(mark);
                    live.wake.notify_all();
                }
                return 1;
            }
            (Some(paths), Some((scope, index))) => {
                paths.iter().any(|path| !scope.holds(path, &index))
            }
        };
        if wanted {
            live.plan(|plan| plan.want(mark));
        }
        u32::from(wanted)
    })
}

/// Builds the list again with the folders macOS guards, once Full Disk Access is switched on
/// while the core runs, and lets text searches sent from then on read them too. `mark` is as
/// for `tl_changed`. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grant_full_disk(core: *const TlCore, mark: u64) {
    guard((), || {
        let core = unsafe { &*core };
        if let Some(text) = &core.text {
            text.full_disk.store(true, Ordering::Relaxed);
        }
        core.live.plan(|plan| {
            plan.full_disk = true;
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
            });
        }
        unsafe { since.write(*mark) };
        drop(watched);
        unsafe { out.write(hand_over(packed)) };
        TL_OK
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
    if !telescope_grep::can_search(&find.query) {
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
            telescope_core::scan_until(&folder, &home, Clutter::Skip, &ignore, find.access, &stale)
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
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Events {
        seen: Mutex<Vec<(u32, u64)>>,
        notes: Mutex<Vec<String>>,
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
        unsafe { tl_submit(core.0, query.as_ptr(), query.len()) }
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
        let text = |start: u32, len: u32| {
            let bytes =
                unsafe { std::slice::from_raw_parts(rows.text.add(start as usize), len as usize) };
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        let copied = unsafe { std::slice::from_raw_parts(rows.rows, rows.count) }
            .iter()
            .map(|row| {
                let name = text(row.name_start, row.name_len);
                (name, text(row.folder_start, row.folder_len), row.is_folder)
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

        let index = telescope_core::stand_in(2_000);
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

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "App")]
    fn app_hits_are_refused_until_a_row_can_show_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Budget.app")).unwrap();
        let apps = Apps::scan_folders(vec![dir.path().to_path_buf()]);
        let index = telescope_core::stand_in(2_000);
        let searcher = Searcher::with_threads(1);
        let hits = searcher
            .search(&index, &apps, "budget", searcher.ticket(), ROWS)
            .unwrap();
        pack(&index, &hits);
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
        let ticket = unsafe { tl_submit(core.0, std::ptr::null(), 0) };
        events.wait_for(TL_EVENT_READY, ticket);
        assert_eq!(take(&core, ticket), Some(Vec::new()));
    }

    fn scan(home: &str, full_disk: u32, events: &Events) -> Core {
        scan_keeping(home, full_disk, 0, events)
    }

    fn scan_keeping(home: &str, full_disk: u32, everything: u32, events: &Events) -> Core {
        let ctx = events as *const Events as *mut c_void;
        let (root, len) = (home.as_ptr(), home.len());
        let core = unsafe { tl_core_new(root, len, full_disk, everything, ctx, Some(record)) };
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
        let list = home.join(".config/telescope/ignore");
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
        assert_eq!(changed(&core, &[], 1), 0);
        unsafe { tl_grant_full_disk(core.0, 1) };
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
        jobs.send(Job::Search { ticket, query }).unwrap();

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
        let not_utf8 = unsafe { tl_core_new([0xff].as_ptr(), 1, 1, 0, ctx, Some(record)) };
        assert!(not_utf8.is_null());
        let no_root = unsafe { tl_core_new(std::ptr::null(), 0, 1, 0, ctx, Some(record)) };
        assert!(no_root.is_null());
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

        // Nothing more changed, so nothing more is built.
        std::thread::sleep(GAP + Duration::from_millis(200));
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
        unsafe { tl_grant_full_disk(core.0, 1) };
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
            text: None,
        })));
        let live = unsafe { &(*core.0).live };
        live.plan(|plan| plan.want(5));
        assert_eq!(changed_at(&core, &[Path::new("/elsewhere")], 0, 7), 1);
        assert_eq!(live.plan.lock().unwrap().asked, Some(5));
        assert_eq!(changed_at(&core, &[Path::new("/elsewhere")], 0, 3), 1);
        assert_eq!(live.plan.lock().unwrap().asked, Some(3));
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
            (path(".config/telescope"), true),
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
        let list = home
            .join(".config/telescope")
            .to_string_lossy()
            .into_owned();
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 0, &events);
        events.wait_for(TL_EVENT_INDEXED, 1);
        assert_eq!(changed(&core, &[&documents], 0), 0);
        assert_eq!(watched_inside(&core), std::slice::from_ref(&list));

        unsafe { tl_grant_full_disk(core.0, 9) };
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
            ".config/telescope/ignore",
        ]);
        std::fs::write(home.join(".config/telescope/ignore"), "notes\n").unwrap();
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
        std::fs::write(home.join(".config/telescope/ignore"), "work/old\n").unwrap();
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

    #[test]
    fn access_granted_before_the_first_list_still_counts() {
        let (_dir, home) = home_with(&["Documents/budget.pdf"]);
        let events = Events::default();
        let core = scan(home.to_str().unwrap(), 0, &events);
        unsafe { tl_grant_full_disk(core.0, 1) };
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

    #[test]
    fn freeing_the_core_cuts_a_waiting_rebuild_short() {
        let events = Events::default();
        let core = start(100, &events);
        assert_eq!(changed(&core, &[], 1), 1);
        events.wait_for_rebuilt(1);
        // This one waits out the gap after the first.
        assert_eq!(changed(&core, &[], 1), 1);
        let freeing = Instant::now();
        drop(core);
        assert!(freeing.elapsed() < GAP / 2, "{:?}", freeing.elapsed());
        assert_eq!(events.rebuilt().len(), 1);
    }

    fn index_of(files: &[&str]) -> (tempfile::TempDir, Arc<Index>) {
        let (dir, home) = home_with(files);
        let ignore = Ignore::default();
        let scan = telescope_core::scan(&home, &home, Clutter::Skip, &ignore, Access::Full);
        (dir, Arc::new(scan.unwrap().index))
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
        })
        .unwrap();
        jobs.send(Job::Swap {
            index: new,
            took: 7,
        })
        .unwrap();
        drop(jobs);
        serve(old, &searcher, &inbox, &done, &notify);
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
            index: newer,
            took: 9,
        })
        .unwrap();
        jobs.send(Job::Search {
            ticket: after,
            query,
        })
        .unwrap();
        drop(jobs);
        let (_dir, unused) = index_of(&[]);
        serve(unused, &searcher, &inbox, &done, &notify);
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
        write(&home.join(".config/telescope/ignore"), "secret\n");
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
        unsafe { tl_grant_full_disk(core.0, 1) };
        let full = grep(&core, &[&home], "budget");
        assert_eq!(grep_all(&core, &events, full).1.lines_found, 1);
    }

    #[test]
    fn a_stand_in_core_reads_guarded_folders_only_once_granted() {
        let events = Events::default();
        let core = start(100, &events);
        let text = unsafe { &*core.0 }.text.as_ref().unwrap();
        assert!(!text.full_disk.load(Ordering::Relaxed));
        unsafe { tl_grant_full_disk(core.0, 1) };
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
            .map(|name| PathBuf::from(format!("/telescope-no-such-root{name}")))
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
    /// `cargo test --release -p telescope-ffi -- --ignored --nocapture name_search_times`.
    #[test]
    #[ignore = "builds a million names and lists a whole folder; slow outside --release"]
    fn name_search_times_while_text_search_runs() {
        let folder =
            std::env::var_os("TL_GREP_FOLDER").map_or_else(telescope_core::home, PathBuf::from);
        let events = Events::default();
        let core = start(1_000_000, &events);
        let queries = [
            "telescope plan",
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

    /// When each rebuild started, whether it was told of Full Disk Access, its mark, and
    /// whether it kept clutter.
    type Starts = Arc<Mutex<Vec<(Instant, bool, u64, Clutter)>>>;

    struct Rebuilds {
        starts: Starts,
        started: Receiver<()>,
        thread: JoinHandle<()>,
    }

    fn rebuilds(
        live: &Arc<Live>,
        gap: Duration,
        last: Instant,
        body: impl Fn() + Send + 'static,
    ) -> Rebuilds {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let (tell, started) = mpsc::channel();
        let thread = {
            let (live, starts) = (Arc::clone(live), Arc::clone(&starts));
            thread::spawn(move || {
                rebuild_loop(&live, gap, last, |full_disk, clutter, mark| {
                    starts
                        .lock()
                        .unwrap()
                        .push((Instant::now(), full_disk, mark, clutter));
                    let _ = tell.send(());
                    body();
                });
            })
        };
        Rebuilds {
            starts,
            started,
            thread,
        }
    }

    impl Rebuilds {
        fn next_starts(&self) {
            let started = self.started.recv_timeout(Duration::from_secs(10));
            assert!(started.is_ok(), "no rebuild within 10 s");
        }

        fn none_starts_within(&self, wait: Duration) {
            assert!(
                self.started.recv_timeout(wait).is_err(),
                "a rebuild started"
            );
        }

        fn stop(self, live: &Live) -> Vec<(Instant, bool, u64, Clutter)> {
            live.plan(|plan| plan.stopped = true);
            self.thread.join().unwrap();
            self.starts.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_lone_change_rebuilds_at_once() {
        let live = Arc::new(Live::default());
        // A gap of a minute, which a lone change long after the last rebuild doesn't wait for.
        let gap = Duration::from_secs(60);
        let long_ago = Instant::now() - gap;
        let rebuilds = rebuilds(&live, gap, long_ago, || {});
        live.plan(|plan| plan.want(4));
        rebuilds.next_starts();

        // The next one, straight after, waits out the gap.
        live.plan(|plan| plan.want(6));
        rebuilds.none_starts_within(Duration::from_millis(300));
        let starts = rebuilds.stop(&live);
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].2, 4);
    }

    #[test]
    fn a_burst_of_changes_rebuilds_a_gap_apart_and_ends_with_the_last_change() {
        let live = Arc::new(Live::default());
        let gap = Duration::from_millis(100);
        let long_ago = Instant::now() - Duration::from_secs(10);
        let takes = Duration::from_millis(30);
        let rebuilds = rebuilds(&live, gap, long_ago, move || thread::sleep(takes));
        let first_change = Instant::now();
        let mut last_change = first_change;
        for _ in 0..50 {
            last_change = Instant::now();
            live.plan(|plan| plan.wanted = true);
            thread::sleep(Duration::from_millis(5));
        }
        let asked = Instant::now();
        let before_last = |starts: &Starts| {
            let starts = starts.lock().unwrap();
            starts.last().is_none_or(|start| start.0 < last_change)
        };
        while before_last(&rebuilds.starts) {
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "no rebuild after the last"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let starts = rebuilds.stop(&live);

        // One at once, at most one per gap while changes keep coming, then the last one.
        let spread = (last_change - first_change).as_millis() / gap.as_millis();
        let most = 3 + spread as usize;
        assert!(
            (2..=most).contains(&starts.len()),
            "{} rebuilds",
            starts.len()
        );
        for pair in starts.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= gap);
        }
    }

    #[test]
    fn changes_during_a_rebuild_wait_as_one_more() {
        let live = Arc::new(Live::default());
        let long_ago = Instant::now() - Duration::from_secs(10);
        // The first rebuild holds until the changes are all in.
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let rebuilds = {
            let gate = Arc::clone(&gate);
            rebuilds(&live, Duration::ZERO, long_ago, move || {
                drop(gate.lock());
            })
        };
        live.plan(|plan| plan.want(1));
        rebuilds.next_starts();
        for mark in 2..22 {
            live.plan(|plan| plan.want(mark));
        }
        drop(held);
        rebuilds.next_starts();
        rebuilds.none_starts_within(Duration::from_millis(300));
        let marks: Vec<u64> = rebuilds.stop(&live).iter().map(|s| s.2).collect();
        assert_eq!(marks, [1, 2]);
    }

    #[test]
    fn a_change_during_a_rebuild_is_judged_by_the_list_it_brings() {
        let (_dir, home) = home_with(&["notes/budget.txt"]);
        let notes = home.join("notes");
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let live = Arc::new(Live::default());
        let index = settings.scan().unwrap().index;
        let scope = Some(settings.scope());
        live.publish(Built { index, scope }, 0);
        let core = Core(Box::into_raw(Box::new(TlCore {
            searcher: Arc::default(),
            jobs: None,
            done: Arc::default(),
            live: Arc::clone(&live),
            worker: None,
            text: None,
        })));
        // Each rebuild holds until let go, and leaves the list as it was.
        let gate = Arc::new(Mutex::new(()));
        let rebuilds = {
            let gate = Arc::clone(&gate);
            let long_ago = Instant::now() - Duration::from_secs(10);
            rebuilds(&live, Duration::ZERO, long_ago, move || drop(gate.lock()))
        };

        let held = gate.lock().unwrap();
        live.plan(|plan| plan.want(1));
        rebuilds.next_starts();
        std::fs::write(notes.join("budget.txt"), "edited").unwrap();
        assert_eq!(changed_at(&core, &[&notes], 0, 2), 1);
        assert!(!live.plan.lock().unwrap().wanted);
        drop(held);
        rebuilds.none_starts_within(Duration::from_millis(300));

        let held = gate.lock().unwrap();
        live.plan(|plan| plan.want(3));
        rebuilds.next_starts();
        std::fs::write(notes.join("plan.md"), "").unwrap();
        assert_eq!(changed_at(&core, &[&notes, &notes], 0, 5), 1);
        assert_eq!(changed_at(&core, &[&notes], 0, 4), 1);
        assert_eq!(
            live.plan.lock().unwrap().recheck,
            Some((vec![notes.clone()], 4))
        );
        drop(held);
        rebuilds.next_starts();
        let marks: Vec<u64> = rebuilds.stop(&live).iter().map(|s| s.2).collect();
        assert_eq!(marks, [1, 3, 4]);
    }

    #[test]
    fn full_disk_access_reaches_one_rebuild() {
        let live = Arc::new(Live::default());
        let long_ago = Instant::now() - Duration::from_secs(10);
        let rebuilds = rebuilds(&live, Duration::ZERO, long_ago, || {});
        live.plan(|plan| {
            plan.full_disk = true;
            plan.wanted = true;
        });
        rebuilds.next_starts();
        live.plan(|plan| plan.wanted = true);
        rebuilds.next_starts();
        let told: Vec<bool> = rebuilds.stop(&live).iter().map(|s| s.1).collect();
        assert_eq!(told, [true, false]);
    }

    /// A core with no threads of its own, whose plan the test drives.
    fn idle_core(live: &Arc<Live>) -> Core {
        Core(Box::into_raw(Box::new(TlCore {
            searcher: Arc::default(),
            jobs: None,
            done: Arc::default(),
            live: Arc::clone(live),
            worker: None,
            text: None,
        })))
    }

    fn switch(core: &Core, on: u32, mark: u64) -> u32 {
        unsafe { tl_index_everything(core.0, on, mark) }
    }

    fn marks_and_clutter(starts: &[(Instant, bool, u64, Clutter)]) -> Vec<(u64, Clutter)> {
        starts.iter().map(|start| (start.2, start.3)).collect()
    }

    #[test]
    fn switching_back_before_the_rebuild_starts_asks_for_none() {
        let live = Arc::new(Live::default());
        let core = idle_core(&live);
        // A list just started, so the next waits out the gap.
        let rebuilds = rebuilds(&live, Duration::from_millis(300), Instant::now(), || {});
        assert_eq!(switch(&core, 1, 3), 1);
        assert_eq!(switch(&core, 1, 4), 0, "already on");
        assert_eq!(switch(&core, 0, 5), 0);
        rebuilds.none_starts_within(Duration::from_millis(600));
        live.plan(|plan| plan.want(7));
        rebuilds.next_starts();
        let starts = rebuilds.stop(&live);
        assert_eq!(marks_and_clutter(&starts), [(7, Clutter::Skip)]);
    }

    #[test]
    fn a_switch_while_a_rebuild_runs_waits_as_one_more() {
        let live = Arc::new(Live::default());
        let core = idle_core(&live);
        let gate = Arc::new(Mutex::new(()));
        let rebuilds = {
            let gate = Arc::clone(&gate);
            let long_ago = Instant::now() - Duration::from_secs(10);
            rebuilds(&live, Duration::ZERO, long_ago, move || drop(gate.lock()))
        };
        let held = gate.lock().unwrap();
        assert_eq!(switch(&core, 1, 3), 1);
        rebuilds.next_starts();
        // Off, on and off again while it runs: only the last differs from what it builds.
        assert_eq!(switch(&core, 0, 4), 1);
        assert_eq!(switch(&core, 1, 5), 0);
        assert_eq!(switch(&core, 0, 6), 1);
        drop(held);
        rebuilds.next_starts();
        rebuilds.none_starts_within(Duration::from_millis(300));
        let starts = rebuilds.stop(&live);
        let expected = [(3, Clutter::Keep), (6, Clutter::Skip)];
        assert_eq!(marks_and_clutter(&starts), expected);
    }

    #[test]
    fn a_change_while_a_rebuild_keeping_everything_runs_waits_for_its_list() {
        let (_dir, home) = home_with(&["notes/a.txt", "Library/Caches/com.example/cache.db"]);
        let caches = home.join("Library/Caches/com.example");
        let settings = Settings::new(&home, &home, Clutter::Skip, Ignore::default(), Access::Full);
        let live = Arc::new(Live::default());
        let index = settings.scan().unwrap().index;
        live.publish(
            Built {
                index,
                scope: Some(settings.scope()),
            },
            0,
        );
        let core = idle_core(&live);
        assert_eq!(changed(&core, &[&caches], 0), 0);
        let gate = Arc::new(Mutex::new(()));
        let rebuilds = {
            let gate = Arc::clone(&gate);
            let long_ago = Instant::now() - Duration::from_secs(10);
            rebuilds(&live, Duration::ZERO, long_ago, move || drop(gate.lock()))
        };
        let held = gate.lock().unwrap();
        assert_eq!(switch(&core, 1, 3), 1);
        rebuilds.next_starts();
        std::fs::write(caches.join("new.db"), "").unwrap();
        assert_eq!(changed_at(&core, &[&caches], 0, 4), 1);
        assert_eq!(
            live.plan.lock().unwrap().recheck,
            Some((vec![caches.clone()], 4))
        );
        drop(held);
        rebuilds.stop(&live);
    }

    #[test]
    fn stopping_wakes_a_rebuild_waiting_out_its_gap() {
        let live = Arc::new(Live::default());
        let rebuilds = rebuilds(&live, Duration::from_secs(60), Instant::now(), || {});
        live.plan(|plan| plan.wanted = true);
        thread::sleep(Duration::from_millis(20));
        let stopping = Instant::now();
        let starts = rebuilds.stop(&live);
        assert!(stopping.elapsed() < Duration::from_secs(1));
        assert!(starts.is_empty());
    }

    #[test]
    fn a_panic_becomes_an_error_code() {
        assert_eq!(
            guard(TL_PANICKED, || panic!("a bug in the engine")),
            TL_PANICKED
        );
    }

    /// Run with `cargo test --release -p telescope-ffi -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds a million names; slow outside --release"]
    fn eight_fast_keystrokes_show_two_results_or_fewer() {
        let events = Events::default();
        let core = start(1_000_000, &events);
        for query in ["telescope plan", "budget notes", "readme", "会議 report"] {
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
        let header = &include_str!("../include/telescope.h").replace('\r', "");

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

        for name in ["TlRow", "TlRows", "TlLine", "TlReport", "TlLines"] {
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
