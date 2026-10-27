//! The hatch: the few C-style functions a window calls to reach the engine. The macOS
//! window calls them from Swift, and the Windows window will reuse them at M5.
//!
//! The window calls in from its main thread and never waits for a search. `tl_submit` hands
//! the search to a worker and returns its ticket at once; the worker calls `on_event` when
//! that ticket's rows are ready; the window then copies them out with `tl_take`.
//! When files change, `tl_changed` asks a second thread to build the list again; the new list
//! reaches the worker through the same queue as the searches, so each search sees one whole list.
//! `include/telescope.h` repeats everything here for Swift, and a test keeps the two in step.

use std::ffi::{CString, c_void};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use telescope_core::{
    Access, Clutter, Hit, Ignore, Index, Scope, Searcher, Settings, Ticket, Watch,
};

/// Raise this with any change below, so a window built against an older header refuses to start.
pub const TL_ABI_VERSION: u32 = 3;

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

const ROWS: usize = 50;

/// Rebuilds start at least this far apart while files keep changing, so a long download
/// doesn't keep a core busy. A change is findable within FSEvents' 0.2 s, plus this, plus
/// one rebuild: about 0.15 s for a home folder.
const GAP: Duration = Duration::from_millis(500);

/// Runs on a Rust thread, never the window's main thread, and must never wait for it.
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

/// The newest finished search, waiting for the window to take it.
type Done = Mutex<Option<(Ticket, Packed)>>;

pub struct TlCore {
    searcher: Arc<Searcher>,
    /// Emptied when the core is freed, which tells the worker to stop.
    jobs: Option<Sender<Job>>,
    done: Arc<Done>,
    live: Arc<Live>,
    worker: Option<JoinHandle<()>>,
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
}

impl Plan {
    fn want(&mut self, mark: u64) {
        self.wanted = true;
        self.asked = Some(self.asked.map_or(mark, |asked| asked.min(mark)));
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
    fn build(&mut self, notify: &Notify) -> io::Result<Built> {
        let (root, access, told) = match self {
            Self::StandIn(count) => {
                let index = telescope_core::stand_in(*count);
                return Ok(Built { index, scope: None });
            }
            Self::Home { root, access, told } => (root, *access, told),
        };
        // The folder being scanned, not $HOME, which a launch from a terminal may change.
        let home = std::fs::canonicalize(root)?;
        // Read again each time, so an edited list counts from the next rebuild on.
        let (ignore, problems) = Ignore::load(&home, access);
        if problems != *told {
            for problem in &problems {
                notify.note(problem);
            }
            *told = problems;
        }
        let settings = Settings::new(&home, &home, Clutter::Skip, ignore, access);
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
/// skipping clutter and the ignore list inside it. Without `full_disk` (0), the folders macOS
/// guards with a pop-up each are left unread. Searches sent before `TL_EVENT_INDEXED` wait for
/// it; a scan that fails sends `TL_EVENT_FAILED`, and each problem with the ignore list a
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
        start(ctx, on_event, Source::Home { root, access, told })
    })
}

/// Starts a core over `count` made-up names, built in the background. Searches sent before
/// `TL_EVENT_INDEXED` wait for it. Returns null on failure, without `on_event`, or for a
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
        start(ctx, on_event, Source::StandIn(count as usize))
    })
}

fn start(ctx: *mut c_void, on_event: Option<TlEventFn>, source: Source) -> *mut TlCore {
    let Some(on_event) = on_event else {
        return std::ptr::null_mut();
    };
    let searcher = Arc::new(Searcher::default());
    let done = Arc::default();
    let live = Arc::default();
    let (jobs, inbox) = mpsc::channel();
    let notify = Notify { ctx, on_event };
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
        source.build(&notify).map(|built| live.publish(built, 0))
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
                rebuild_loop(&live, GAP, started, |full_disk, mark| {
                    rebuild(&mut source, full_disk, mark, &live, &swaps, &notify);
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
        let Some(hits) = searcher.search(&index, &query, ticket, ROWS) else {
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
/// and the mark of the first change it reads.
fn rebuild_loop(live: &Live, gap: Duration, mut last: Instant, mut rebuild: impl FnMut(bool, u64)) {
    let mut plan = live.plan.lock().unwrap();
    loop {
        if plan.stopped {
            return;
        }
        if !plan.wanted {
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
        let mark = plan.asked.take().unwrap_or_default();
        drop(plan);
        last = now;
        rebuild(full_disk, mark);
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
        source.build(notify).map(|built| live.publish(built, mark))
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
    for hit in hits {
        let path = index.path(hit.entry);
        let folder = path.parent().unwrap_or(&path).to_string_lossy();
        let name_start = packed.text.len() as u32;
        packed.text.push_str(index.name(hit.entry));
        let folder_start = packed.text.len() as u32;
        packed.text.push_str(&folder);
        packed.rows.push(TlRow {
            name_start,
            name_len: folder_start - name_start,
            folder_start,
            folder_len: folder.len() as u32,
            is_folder: index.is_dir(hit.entry),
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
        let running = {
            let mut plan = live.plan.lock().unwrap();
            if plan.stopped {
                return 0;
            }
            // A rebuild not started yet will read every change made before it starts.
            if plan.wanted {
                plan.want(mark);
                return 1;
            }
            plan.running
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
                // The rebuild under way may have read these folders before they changed.
                let changed: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|path| scope.in_scope(path) || !scope.holds(path, &index))
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
/// while the core runs. `mark` is as for `tl_changed`. Safe to call from any thread.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tl_grant_full_disk(core: *const TlCore, mark: u64) {
    guard((), || {
        let core = unsafe { &*core };
        core.live.plan(|plan| {
            plan.full_disk = true;
            plan.want(mark);
        });
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

/// Stops the core and waits for its threads, so `on_event` never runs once this returns.
/// Called while a list is being built, it waits for the build to finish: under 0.2 s for a
/// million made-up names, but a home folder scanned with nothing cached may take longer.
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
    }

    impl Events {
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
            .search(&index, "budget", searcher.ticket(), ROWS)
            .unwrap()
            .iter()
            .map(|hit| {
                let path = index.path(hit.entry);
                let folder = path.parent().unwrap().to_string_lossy().into_owned();
                (
                    index.name(hit.entry).to_owned(),
                    folder,
                    index.is_dir(hit.entry),
                )
            })
            .collect();
        assert_eq!(expected.len(), ROWS);
        assert_eq!(take(&core, ticket), Some(expected));
        assert_eq!(take(&core, ticket), None);
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
        let ctx = events as *const Events as *mut c_void;
        let core = unsafe { tl_core_new(home.as_ptr(), home.len(), full_disk, ctx, Some(record)) };
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
        let not_utf8 = unsafe { tl_core_new([0xff].as_ptr(), 1, 1, ctx, Some(record)) };
        assert!(not_utf8.is_null());
        let no_root = unsafe { tl_core_new(std::ptr::null(), 0, 1, ctx, Some(record)) };
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

    /// When each rebuild started, whether it was told of Full Disk Access, and its mark.
    type Starts = Arc<Mutex<Vec<(Instant, bool, u64)>>>;

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
                rebuild_loop(&live, gap, last, |full_disk, mark| {
                    starts
                        .lock()
                        .unwrap()
                        .push((Instant::now(), full_disk, mark));
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

        fn stop(self, live: &Live) -> Vec<(Instant, bool, u64)> {
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

        for name in ["TlRow", "TlRows"] {
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
