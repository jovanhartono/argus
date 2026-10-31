//! Spotlight, macOS's own index of the words inside files: it names the files that may hold
//! the typed words, so a search reads only those instead of every file.

use std::path::PathBuf;

/// Spotlight names at most this many files per search. Uncapped, "i*" named 427,775 files in
/// 33.5 s; capped at 200 it took 0.6 s (31 Oct).
pub const SPOTLIGHT_FILES: usize = 1000;

/// The Spotlight query for `text`: each typed word must start a word in the file, in any case
/// and with or without accents. Punctuation around a word is left off, as Spotlight splits
/// words there. `None` when no word holds a letter or digit.
pub fn query(text: &str) -> Option<String> {
    let mut query = String::new();
    let words = text
        .split_whitespace()
        .map(|word| word.trim_matches(|letter: char| !letter.is_alphanumeric()))
        .filter(|word| !word.is_empty());
    for word in words {
        if !query.is_empty() {
            query.push_str(" && ");
        }
        query.push_str("kMDItemTextContent == \"");
        for letter in word.chars() {
            if matches!(letter, '\\' | '"' | '*' | '?') {
                query.push('\\');
            }
            query.push(letter);
        }
        query.push_str("*\"cdw");
    }
    (!query.is_empty()).then_some(query)
}

/// `query` narrowed to the files changed in the last week.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn recent(query: &str) -> String {
    format!("({query}) && kMDItemFSContentChangeDate >= $time.today(-7)")
}

/// The files inside `scopes` that Spotlight says hold the words of `text`, in no useful order:
/// at most `SPOTLIGHT_FILES`, and when that many come, also up to as many changed in the last
/// week. Asks again every 10 ms whether the search is `stale`, and then stops Spotlight's own
/// work too and returns `None`. Runs a run loop on this thread while it waits, so it never
/// runs on the main thread.
pub fn files(text: &str, scopes: &[PathBuf], stale: &dyn Fn() -> bool) -> Option<Vec<PathBuf>> {
    files_up_to(SPOTLIGHT_FILES, text, scopes, stale)
}

fn files_up_to(
    cap: usize,
    text: &str,
    scopes: &[PathBuf],
    stale: &dyn Fn() -> bool,
) -> Option<Vec<PathBuf>> {
    let Some(query) = query(text) else {
        return Some(Vec::new());
    };
    #[cfg(target_os = "macos")]
    {
        let mut found = mac::files(&query, scopes, cap, stale)?;
        // A common word fills the cap with whatever Spotlight met first, which can leave out
        // the invoice you edited yesterday.
        if found.len() >= cap {
            let recent = mac::files(&recent(&query), scopes, cap, stale)?;
            let have: std::collections::HashSet<&PathBuf> = found.iter().collect();
            let fresh: Vec<PathBuf> = recent
                .into_iter()
                .filter(|file| !have.contains(file))
                .collect();
            found.extend(fresh);
        }
        Some(found)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (query, scopes, cap, stale);
        Some(Vec::new())
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::path::PathBuf;

    type CFTypeRef = *const c_void;
    type CFIndex = isize;
    type Boolean = u8;

    #[repr(C)]
    struct CFRange {
        location: CFIndex,
        length: CFIndex,
    }

    #[repr(C)]
    struct Opaque {
        _private: [u8; 0],
    }

    const UTF8: u32 = 0x0800_0100;
    const RUN_FINISHED: i32 = 1;

    // SAFETY: the signatures match CoreFoundation's CFBase.h, CFString.h, CFArray.h and
    // CFRunLoop.h, and libobjc's autorelease pool calls.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeArrayCallBacks: Opaque;
        static kCFRunLoopDefaultMode: CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            len: CFIndex,
            encoding: u32,
            external: Boolean,
        ) -> CFTypeRef;
        fn CFStringGetLength(string: CFTypeRef) -> CFIndex;
        fn CFStringGetBytes(
            string: CFTypeRef,
            range: CFRange,
            encoding: u32,
            loss: u8,
            external: Boolean,
            buffer: *mut u8,
            buffer_len: CFIndex,
            used: *mut CFIndex,
        ) -> CFIndex;
        fn CFArrayCreate(
            alloc: CFTypeRef,
            values: *const CFTypeRef,
            count: CFIndex,
            callbacks: *const Opaque,
        ) -> CFTypeRef;
        fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, once: Boolean) -> i32;
    }

    // SAFETY: the signatures match CoreServices' MDQuery.h and MDItem.h.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        static kMDItemPath: CFTypeRef;
        fn MDQueryCreate(
            alloc: CFTypeRef,
            query: CFTypeRef,
            values: CFTypeRef,
            sorting: CFTypeRef,
        ) -> CFTypeRef;
        fn MDQuerySetSearchScope(query: CFTypeRef, scope: CFTypeRef, options: u32);
        fn MDQuerySetMaxCount(query: CFTypeRef, count: CFIndex);
        fn MDQueryExecute(query: CFTypeRef, flags: usize) -> Boolean;
        fn MDQueryStop(query: CFTypeRef);
        fn MDQueryIsGatheringComplete(query: CFTypeRef) -> Boolean;
        fn MDQueryGetResultCount(query: CFTypeRef) -> CFIndex;
        fn MDQueryGetResultAtIndex(query: CFTypeRef, index: CFIndex) -> CFTypeRef;
        fn MDItemCopyAttribute(item: CFTypeRef, name: CFTypeRef) -> CFTypeRef;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    /// A CoreFoundation object this code created, released once.
    struct Owned(CFTypeRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: made by a Create call, so this code holds one reference to it.
            unsafe { CFRelease(self.0) };
        }
    }

    fn string(text: &str) -> Option<Owned> {
        // SAFETY: the bytes are valid UTF-8 for their length; a null comes back on failure.
        let made = unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                text.as_ptr(),
                text.len() as CFIndex,
                UTF8,
                0,
            )
        };
        (!made.is_null()).then_some(Owned(made))
    }

    fn array(items: &[CFTypeRef]) -> Owned {
        // SAFETY: each item is a live CoreFoundation object, retained by the array.
        Owned(unsafe {
            CFArrayCreate(
                std::ptr::null(),
                items.as_ptr(),
                items.len() as CFIndex,
                &kCFTypeArrayCallBacks,
            )
        })
    }

    /// A Spotlight answer as a path; `None` for anything but a string.
    ///
    /// # Safety
    /// `value` is null or a live CoreFoundation object.
    unsafe fn path(value: CFTypeRef) -> Option<PathBuf> {
        if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
            return None;
        }
        let range = || CFRange {
            location: 0,
            length: unsafe { CFStringGetLength(value) },
        };
        let mut used = 0;
        // SAFETY: a null buffer only measures; the second call writes at most `used` bytes.
        unsafe {
            CFStringGetBytes(
                value,
                range(),
                UTF8,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut used,
            )
        };
        let mut bytes = vec![0; used as usize];
        unsafe {
            CFStringGetBytes(
                value,
                range(),
                UTF8,
                0,
                0,
                bytes.as_mut_ptr(),
                used,
                &mut used,
            )
        };
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }

    pub(super) fn files(
        query: &str,
        scopes: &[PathBuf],
        cap: usize,
        stale: &dyn Fn() -> bool,
    ) -> Option<Vec<PathBuf>> {
        if scopes.is_empty() {
            return Some(Vec::new());
        }
        // Spotlight's own objects, freed after each search on a thread that lives on.
        // SAFETY: popped below with the pool pushed here.
        let pool = unsafe { objc_autoreleasePoolPush() };
        let found = ask(query, scopes, cap, stale);
        unsafe { objc_autoreleasePoolPop(pool) };
        found
    }

    fn ask(
        query: &str,
        scopes: &[PathBuf],
        cap: usize,
        stale: &dyn Fn() -> bool,
    ) -> Option<Vec<PathBuf>> {
        let Some(text) = string(query) else {
            return Some(Vec::new());
        };
        let folders: Vec<Owned> = scopes
            .iter()
            .filter_map(|scope| scope.to_str().and_then(string))
            .collect();
        let scope = array(&folders.iter().map(|folder| folder.0).collect::<Vec<_>>());
        // SAFETY: every argument is a live object or null where MDQuery.h allows one; a query
        // Spotlight can't read comes back null.
        let asked =
            unsafe { MDQueryCreate(std::ptr::null(), text.0, std::ptr::null(), std::ptr::null()) };
        if asked.is_null() {
            return Some(Vec::new());
        }
        let asked = Owned(asked);
        unsafe {
            MDQuerySetSearchScope(asked.0, scope.0, 0);
            MDQuerySetMaxCount(asked.0, cap as CFIndex);
            // Without kMDQuerySynchronous it gathers in the background, telling this thread's
            // run loop, so it can be stopped midway.
            if MDQueryExecute(asked.0, 0) == 0 {
                return Some(Vec::new());
            }
        }
        // SAFETY: the query is live until `asked` drops, after its last use here.
        while unsafe { MDQueryIsGatheringComplete(asked.0) } == 0 {
            if stale() {
                // Ends Spotlight's work too: a client that just went away left it busy for
                // seconds more.
                unsafe { MDQueryStop(asked.0) };
                return None;
            }
            if unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, 0) } == RUN_FINISHED {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        unsafe { MDQueryStop(asked.0) };
        let count = unsafe { MDQueryGetResultCount(asked.0) };
        let paths = (0..count).filter_map(|at| {
            // SAFETY: `at` is below the count; the item belongs to the query, and the path
            // copied from it is this code's to release.
            let item = unsafe { MDQueryGetResultAtIndex(asked.0, at) };
            if item.is_null() {
                return None;
            }
            let copied = unsafe { MDItemCopyAttribute(item, kMDItemPath) };
            if copied.is_null() {
                return None;
            }
            let copied = Owned(copied);
            unsafe { path(copied.0) }
        });
        Some(paths.collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_word_must_start_a_word_in_the_file() {
        assert_eq!(
            query("invoice").as_deref(),
            Some(r#"kMDItemTextContent == "invoice*"cdw"#)
        );
        assert_eq!(
            query("  refund   diskon ").as_deref(),
            Some(r#"kMDItemTextContent == "refund*"cdw && kMDItemTextContent == "diskon*"cdw"#)
        );
        assert_eq!(
            query("École 3857").as_deref(),
            Some(r#"kMDItemTextContent == "École*"cdw && kMDItemTextContent == "3857*"cdw"#)
        );
    }

    #[test]
    fn spotlights_own_marks_are_escaped() {
        assert_eq!(
            query(r#"a"b a\b a*b a?b"#).as_deref(),
            Some(concat!(
                r#"kMDItemTextContent == "a\"b*"cdw && kMDItemTextContent == "a\\b*"cdw && "#,
                r#"kMDItemTextContent == "a\*b*"cdw && kMDItemTextContent == "a\?b*"cdw"#
            ))
        );
    }

    #[test]
    fn punctuation_around_a_word_is_left_off_and_alone_is_dropped() {
        assert_eq!(
            query("f(x) -- \"quoted\" tl_grep_submit(").as_deref(),
            Some(concat!(
                r#"kMDItemTextContent == "f(x*"cdw && kMDItemTextContent == "quoted*"cdw && "#,
                r#"kMDItemTextContent == "tl_grep_submit*"cdw"#
            ))
        );
    }

    #[test]
    fn the_recent_ask_keeps_every_word() {
        assert_eq!(
            recent(&query("refund diskon").unwrap()),
            concat!(
                r#"(kMDItemTextContent == "refund*"cdw && kMDItemTextContent == "diskon*"cdw)"#,
                " && kMDItemFSContentChangeDate >= $time.today(-7)"
            )
        );
    }

    #[test]
    fn nothing_to_ask_without_a_letter_or_digit() {
        assert_eq!(query(""), None);
        assert_eq!(query("   "), None);
        assert_eq!(query("-- ( ) *?"), None);
        assert_eq!(files("--", &[PathBuf::from("/")], &|| false), Some(vec![]));
    }

    /// Run with `cargo test -p argus-grep --release -- --ignored --nocapture spotlight`; needs
    /// Spotlight on and a home folder it has read.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "asks this Mac's Spotlight"]
    fn asks_spotlight_from_a_worker_thread_and_stops_when_stale() {
        let home = argus_core::home();
        let found = std::thread::spawn(move || files("the", &[home], &|| false))
            .join()
            .unwrap()
            .unwrap();
        println!("\"the\": {} files", found.len());
        assert!(found.len() <= 2 * SPOTLIGHT_FILES);
        let unique: std::collections::HashSet<&PathBuf> = found.iter().collect();
        assert_eq!(unique.len(), found.len());
        assert!(found.iter().all(|path| path.is_absolute()));
        let started = std::time::Instant::now();
        let stopped = files("a", &[argus_core::home()], &|| {
            started.elapsed() > std::time::Duration::from_millis(50)
        });
        assert_eq!(stopped, None);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    /// How long Spotlight takes to name the files, by cap and scope, over the window bench's
    /// words. Run with `cargo test -p argus-grep --release -- --ignored --nocapture
    /// spotlight_times`; check `uptime` first.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "measures this Mac's Spotlight"]
    fn spotlight_times() {
        use std::time::Instant;
        let home = argus_core::home();
        // Near enough to the folders ⌘T keeps: no hidden ones, no Library.
        let mut kept: Vec<PathBuf> = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                !name.starts_with('.') && name != "Library"
            })
            .collect();
        kept.sort();
        let words = [
            "nib",
            "iumk",
            "3857",
            "bulk webp rec",
            "refund diskon",
            "otel metrics",
            "pos keynote",
            "payment mix",
            "insurance claim",
            "cv ats",
            "a",
            "in",
            "inv",
        ];
        let scopes = [("~", vec![home.clone()]), ("kept", kept)];
        for cap in [200, 500, 1000, 2000] {
            for (name, scope) in &scopes {
                let mut times = Vec::new();
                for text in words {
                    let started = Instant::now();
                    let found = files_up_to(cap, text, scope, &|| false).unwrap();
                    let took = started.elapsed().as_secs_f64() * 1000.0;
                    println!(
                        "cap {cap} {name} {text:?}: {} files, {took:.0} ms",
                        found.len()
                    );
                    times.push(took);
                }
                times.sort_by(f64::total_cmp);
                let at = |share: f64| times[((times.len() - 1) as f64 * share).round() as usize];
                println!(
                    "cap {cap} {name}: p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms",
                    at(0.5),
                    at(0.95),
                    at(1.0)
                );
            }
        }
        for stop_at in [125, u64::MAX] {
            let started = Instant::now();
            let stale =
                || started.elapsed() > std::time::Duration::from_millis(stop_at.min(60_000));
            let found = files_up_to(SPOTLIGHT_FILES, "de", std::slice::from_ref(&home), &stale);
            println!(
                "\"de\" stopped at {stop_at} ms: {:?} files after {:.0} ms",
                found.map(|found| found.len()),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
