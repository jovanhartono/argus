//! The hatch: the few C-style functions a window calls to reach the engine. The macOS
//! window calls them from Swift, and the Windows window will reuse them at M5.
//!
//! The window calls in from its main thread and never waits for a search. `tl_submit` hands
//! the search to a worker and returns its ticket at once; the worker calls `on_event` when
//! that ticket's rows are ready; the window then copies them out with `tl_take`.
//! `include/telescope.h` repeats everything here for Swift, and a test keeps the two in step.

use std::ffi::{CString, c_void};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use telescope_core::{Access, Clutter, Hit, Ignore, Index, Searcher, Ticket};

/// Raise this with any change below, so a window built against an older header refuses to start.
pub const TL_ABI_VERSION: u32 = 2;

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

const ROWS: usize = 50;

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
    worker: Option<JoinHandle<()>>,
}

struct Job {
    ticket: Ticket,
    query: String,
}

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
        start(ctx, on_event, move |notify| {
            // The folder being scanned, not $HOME, which a launch from a terminal may change.
            let home = std::fs::canonicalize(root)?;
            let (ignore, problems) = Ignore::load(&home, access);
            for problem in problems {
                notify.note(&problem);
            }
            telescope_core::scan(&home, &home, Clutter::Skip, &ignore, access)
                .map(|scan| scan.index)
        })
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
        start(ctx, on_event, move |_| {
            Ok(telescope_core::stand_in(count as usize))
        })
    })
}

fn start(
    ctx: *mut c_void,
    on_event: Option<TlEventFn>,
    build: impl FnOnce(&Notify) -> io::Result<Index> + Send + 'static,
) -> *mut TlCore {
    let Some(on_event) = on_event else {
        return std::ptr::null_mut();
    };
    let searcher = Arc::new(Searcher::default());
    let done = Arc::default();
    let (jobs, inbox) = mpsc::channel();
    let notify = Notify { ctx, on_event };
    let worker = {
        let (searcher, done) = (Arc::clone(&searcher), Arc::clone(&done));
        thread::Builder::new()
            .name("hatch".into())
            .spawn(move || work(build, &searcher, &inbox, &done, &notify))
            .expect("start the hatch worker")
    };
    Box::into_raw(Box::new(TlCore {
        searcher,
        jobs: Some(jobs),
        done,
        worker: Some(worker),
    }))
}

fn work(
    build: impl FnOnce(&Notify) -> io::Result<Index>,
    searcher: &Searcher,
    inbox: &Receiver<Job>,
    done: &Done,
    notify: &Notify,
) {
    let stopped_cleanly = panic::catch_unwind(AssertUnwindSafe(|| {
        let index = match build(notify) {
            Ok(index) => index,
            Err(error) => {
                let code = error.raw_os_error().map_or(0, |code| code as u64);
                notify.send(TL_EVENT_FAILED, code);
                return;
            }
        };
        notify.send(TL_EVENT_INDEXED, index.len() as u64);
        while let Ok(job) = inbox.recv() {
            // A search replaced by a newer keystroke gives up and is never shown.
            let Some(hits) = searcher.search(&index, &job.query, job.ticket, ROWS) else {
                continue;
            };
            // A key pressed just as the search finished: skip rows the window would refuse.
            if searcher.newest() != job.ticket.number() {
                continue;
            }
            *done.lock().unwrap() = Some((job.ticket, pack(&index, &hits)));
            notify.send(TL_EVENT_READY, job.ticket.number());
        }
    }));
    if stopped_cleanly.is_err() {
        notify.send(TL_EVENT_FAILED, 0);
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
            .is_some_and(|jobs| jobs.send(Job { ticket, query }).is_ok());
        if sent { ticket.number() } else { 0 }
    })
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
        let packed = Box::new(packed);
        let rows = TlRows {
            rows: packed.rows.as_ptr(),
            count: packed.rows.len(),
            text: packed.text.as_ptr(),
            owner: Box::into_raw(packed).cast(),
        };
        unsafe { out.write(rows) };
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

/// Stops the core and waits for its worker, so `on_event` never runs once this returns.
/// Called while the index is still being built, it waits for the build to finish: under 0.2 s
/// for a million made-up names, but a home folder scanned with nothing cached may take longer.
///
/// # Safety
/// `core` came from `tl_core_new` or `tl_core_new_stand_in` and is freed once, never from inside `on_event`.
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

        fn ready(&self) -> Vec<u64> {
            let seen = self.seen.lock().unwrap();
            seen.iter()
                .filter(|(event, _)| *event == TL_EVENT_READY)
                .map(|&(_, ticket)| ticket)
                .collect()
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
        let rows = unsafe { rows.assume_init() };
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
        Some(copied)
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
            events.wait_for(TL_EVENT_FAILED, code);
            assert_eq!(*events.seen.lock().unwrap(), [(TL_EVENT_FAILED, code)]);
        }
    }

    #[test]
    fn problems_with_the_ignore_list_are_told() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(home.join(".config/telescope")).unwrap();
        std::fs::write(home.join(".config/telescope/ignore"), "work/old\n").unwrap();

        let events = Events::default();
        let _core = scan(home.to_str().unwrap(), 1, &events);
        events.wait_for(TL_EVENT_INDEXED, 1);
        let notes = events.notes.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("line 1"), "{}", notes[0]);
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
