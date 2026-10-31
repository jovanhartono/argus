//! Argus's text search: finds typed text inside the text and code files of a scanned
//! folder, or of the files Spotlight names, on threads of its own so name results never wait
//! for it.

mod cloud;
mod literal;
mod priority;
pub mod spotlight;

pub use argus_core::Ticket;
use argus_core::{EntryId, Index, Tickets};
use grep_matcher::Matcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use literal::Literal;
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::num::NonZero;
use std::ops::{Add, Range};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Bigger files are logs, dumps and data, rarely where someone looks for a word.
pub const MAX_FILE_BYTES: u64 = 8 << 20;
/// A longer line, like a minified script, ends that file's search, so a thread never holds
/// more than this much of a file.
const MAX_LINE_BYTES: usize = 1 << 20;
/// How much of a long line a result shows, and how much of it comes before the match.
pub const MAX_TEXT_BYTES: usize = 200;
const BEFORE_MATCH: usize = 40;
/// A file shows at most this many lines, so one long log can't fill the list.
pub const LINES_PER_FILE: usize = 3;

/// One matching line, lent to the caller while it is handed over.
#[derive(Debug)]
pub struct Found<'a> {
    pub path: &'a Path,
    /// Counted from 1.
    pub line: u64,
    /// The line without its surrounding spaces, cut to about `MAX_TEXT_BYTES` around the match.
    pub text: &'a str,
    /// Where the match sits in `text`.
    pub matched: Range<usize>,
}

/// What the search did with each file, so skipped files can be shown. Each file counts once,
/// and a file that showed lines counts as searched even when a later zero byte or long line
/// ended it. A file still open when the search ended counts only if it showed lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub files_searched: usize,
    pub lines_found: usize,
    /// Files holding a zero byte, like images and archives.
    pub binary: usize,
    /// Files over `MAX_FILE_BYTES`, or with a line over a megabyte.
    pub too_large: usize,
    /// Files kept only in the cloud, never opened.
    pub cloud_only: usize,
    pub unreadable: usize,
}

impl Add for Report {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            files_searched: self.files_searched + other.files_searched,
            lines_found: self.lines_found + other.lines_found,
            binary: self.binary + other.binary,
            too_large: self.too_large + other.too_large,
            cloud_only: self.cloud_only + other.cloud_only,
            unreadable: self.unreadable + other.unreadable,
        }
    }
}

pub struct Grep {
    pool: rayon::ThreadPool,
    tickets: Tickets,
    /// One search at a time, so a burst of keystrokes never reads several sets of files.
    turn: Mutex<()>,
    cloud_only: fn(&Path, &Metadata) -> bool,
}

impl Default for Grep {
    /// One thread per core minus one, so the window always has a core free.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, NonZero::get);
        Self::with_threads((cores - 1).max(1))
    }
}

impl Grep {
    pub fn with_threads(threads: usize) -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("grep-{i}"))
            .start_handler(|_| {
                cloud::never_download();
                priority::yield_to_names();
            })
            .build()
            .expect("start text search threads");
        Self {
            pool,
            tickets: Tickets::default(),
            turn: Mutex::new(()),
            cloud_only: cloud::is_cloud_only,
        }
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Take one per keystroke. Every search holding an older ticket gives up.
    pub fn ticket(&self) -> Ticket {
        self.tickets.take()
    }

    /// The number of the last ticket handed out; only its search is still wanted.
    pub fn newest(&self) -> u64 {
        self.tickets.newest()
    }

    /// Runs `work`, and the rayon jobs it starts, on the text search threads, below name search
    /// and with downloads off, as for listing the folders to search.
    pub fn install<R: Send>(&self, work: impl FnOnce() -> R + Send) -> R {
        self.pool.install(work)
    }

    /// Hands each file's matching lines, up to `LINES_PER_FILE`, to `found` in one call once
    /// the file is read, from several threads at once and in no set order, until `limit` lines.
    /// Upper-case letters in `text` must match exactly; lower-case ones match either case.
    /// Returns `None` when a newer ticket arrived first, though some lines may have been
    /// handed over by then.
    pub fn search(
        &self,
        index: &Index,
        text: &str,
        ticket: Ticket,
        limit: usize,
        found: impl Fn(&[Found<'_>]) + Sync,
    ) -> Option<Report> {
        self.search_until(index, text, ticket, limit, &|| false, found)
    }

    /// As `search`, but once `stop` says so no more files are read and none stays open, as for
    /// a drive being ejected; the report then counts the files read before.
    pub fn search_until(
        &self,
        index: &Index,
        text: &str,
        ticket: Ticket,
        limit: usize,
        stop: &(dyn Fn() -> bool + Sync),
        found: impl Fn(&[Found<'_>]) + Sync,
    ) -> Option<Report> {
        self.run(text, ticket, limit, stop, |job| {
            self.pool.install(|| {
                index
                    .ids()
                    .into_par_iter()
                    .try_for_each_init(searcher, |searcher, entry| {
                        job.entry(index, entry, searcher, &found)
                    })
            });
        })
    }

    /// As `search_until`, for `files` in the order given, as Spotlight's best first: a few
    /// are read at once and handed over in that order, so the first files' lines fill the
    /// limit and come first.
    pub fn search_files(
        &self,
        files: &[PathBuf],
        text: &str,
        ticket: Ticket,
        limit: usize,
        stop: &(dyn Fn() -> bool + Sync),
        found: impl Fn(&[Found<'_>]),
    ) -> Option<Report> {
        let group = 2 * self.threads();
        self.run(text, ticket, limit, stop, |job| {
            for files in files.chunks(group) {
                if (job.stale)() || job.done() {
                    break;
                }
                let read: Vec<_> = self.pool.install(|| {
                    files
                        .par_iter()
                        .map_init(searcher, |searcher, path| job.file(path, searcher))
                        .collect()
                });
                for (path, (report, lines)) in files.iter().zip(read) {
                    job.hand_over(path, report, &lines, &found);
                }
            }
        })
    }

    fn run(
        &self,
        text: &str,
        ticket: Ticket,
        limit: usize,
        stop: &(dyn Fn() -> bool + Sync),
        read: impl FnOnce(&Job<'_>),
    ) -> Option<Report> {
        let stale = || self.tickets.is_stale(ticket);
        // A panic in `found` leaves nothing half-done behind the lock, so later searches go on.
        let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        if stale() {
            return None;
        }
        let Some(matcher) = Literal::new(text) else {
            return Some(Report::default());
        };
        let quit = || stale() || stop();
        let job = Job {
            matcher: &matcher,
            limit,
            delivered: AtomicUsize::new(0),
            report: Mutex::default(),
            stale: &quit,
            cloud_only: self.cloud_only,
        };
        read(&job);
        let report = job.report.into_inner().unwrap();
        (!stale()).then_some(report)
    }
}

/// Whether `text` can be looked for: not empty, and on one line. Other text finds nothing.
pub fn can_search(text: &str) -> bool {
    !text.is_empty() && !text.contains('\n')
}

fn searcher() -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .heap_limit(Some(MAX_LINE_BYTES))
        .build()
}

struct Job<'a> {
    matcher: &'a Literal,
    limit: usize,
    /// Lines handed over, or about to be.
    delivered: AtomicUsize,
    /// Kept here rather than folded, so files finished while the walk stops still count.
    report: Mutex<Report>,
    stale: &'a (dyn Fn() -> bool + Sync),
    cloud_only: fn(&Path, &Metadata) -> bool,
}

/// A matching line, kept until its file's lines are handed over together.
struct Line {
    line: u64,
    text: String,
    matched: Range<usize>,
}

impl Job<'_> {
    fn done(&self) -> bool {
        self.delivered.load(Ordering::Relaxed) >= self.limit
    }

    /// `None` stops the walk, once the search is no longer wanted or has all its lines.
    fn entry(
        &self,
        index: &Index,
        entry: EntryId,
        searcher: &mut Searcher,
        found: &impl Fn(&[Found<'_>]),
    ) -> Option<()> {
        if (self.stale)() || self.done() {
            return None;
        }
        if !index.is_dir(entry) && !index.is_removed(entry) {
            let path = index.path(entry);
            let (report, lines) = self.file(&path, searcher);
            self.hand_over(&path, report, &lines, found);
        }
        Some(())
    }

    /// Hands over as many of a file's `lines` as the limit leaves room for, while the search
    /// is still wanted, and counts the file.
    fn hand_over(
        &self,
        path: &Path,
        mut report: Report,
        lines: &[Line],
        found: &impl Fn(&[Found<'_>]),
    ) {
        if !lines.is_empty() {
            let room = if (self.stale)() {
                0
            } else {
                let before = self.delivered.fetch_add(lines.len(), Ordering::Relaxed);
                self.limit.saturating_sub(before).min(lines.len())
            };
            if room == 0 {
                // Ended by the search before any line: neither searched nor skipped.
                report = Report::default();
            } else {
                let shown: Vec<Found<'_>> = lines[..room]
                    .iter()
                    .map(|line| Found {
                        path,
                        line: line.line,
                        text: &line.text,
                        matched: line.matched.clone(),
                    })
                    .collect();
                found(&shown);
                report.lines_found = room;
            }
        }
        let mut total = self.report.lock().unwrap();
        *total = *total + report;
    }

    /// Opening failed, or reading did with downloads off: the file may have just been moved to
    /// the cloud.
    fn failed(&self, path: &Path) -> Report {
        let cloud_only = fs::symlink_metadata(path).is_ok_and(|now| (self.cloud_only)(path, &now));
        Report {
            cloud_only: usize::from(cloud_only),
            unreadable: usize::from(!cloud_only),
            ..Report::default()
        }
    }

    /// Reads the file's first `LINES_PER_FILE` matching lines. The report counts no lines; the
    /// hand-over does.
    fn file(&self, path: &Path, searcher: &mut Searcher) -> (Report, Vec<Line>) {
        let mut report = Report::default();
        // Links are not followed, as in the scan.
        let Ok(metadata) = fs::symlink_metadata(path) else {
            report.unreadable = 1;
            return (report, Vec::new());
        };
        if !metadata.is_file() {
            return (report, Vec::new());
        }
        // Checked before opening: opening a cloud-only file can start its download.
        if (self.cloud_only)(path, &metadata) {
            report.cloud_only = 1;
            return (report, Vec::new());
        }
        if metadata.len() > MAX_FILE_BYTES {
            report.too_large = 1;
            return (report, Vec::new());
        }
        let Ok(file) = File::open(path) else {
            return (self.failed(path), Vec::new());
        };
        let mut sink = Lines {
            job: self,
            lines: Vec::new(),
            binary: false,
            stopped: false,
        };
        let mut reader = Stoppable {
            file,
            job: self,
            stopped: false,
        };
        match searcher.search_reader(self.matcher, &mut reader, &mut sink) {
            _ if !sink.lines.is_empty() => report.files_searched = 1,
            // Ended by the search before any line: neither searched nor skipped.
            _ if reader.stopped || sink.stopped => {}
            Ok(()) if sink.binary => report.binary = 1,
            Ok(()) => report.files_searched = 1,
            // The searcher's own errors carry no system code; its only one here is a line
            // too long to hold.
            Err(error) if error.raw_os_error().is_none() => report.too_large = 1,
            Err(_) => report = self.failed(path),
        }
        (report, sink.lines)
    }
}

struct Stoppable<'a> {
    file: File,
    job: &'a Job<'a>,
    stopped: bool,
}

impl Read for Stoppable<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.job.stale)() || self.job.done() {
            self.stopped = true;
            return Ok(0);
        }
        self.file.read(buf)
    }
}

struct Lines<'a> {
    job: &'a Job<'a>,
    lines: Vec<Line>,
    binary: bool,
    stopped: bool,
}

impl Sink for Lines<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, line: &SinkMatch<'_>) -> io::Result<bool> {
        if (self.job.stale)() || self.job.done() {
            self.stopped = true;
            return Ok(false);
        }
        let (text, matched) = excerpt(self.job.matcher, line.bytes());
        self.lines.push(Line {
            line: line.line_number().unwrap_or(0),
            text: text.into_owned(),
            matched,
        });
        Ok(self.lines.len() < LINES_PER_FILE)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> io::Result<bool> {
        self.binary = true;
        Ok(false)
    }
}

/// The line without its surrounding spaces, cut to a window around the match when long.
fn excerpt<'b>(matcher: &Literal, line: &'b [u8]) -> (Cow<'b, str>, Range<usize>) {
    let untrimmed = line;
    let lead = line.len() - line.trim_ascii_start().len();
    let line = line.trim_ascii();
    let found = shown_match(matcher, untrimmed, lead..lead + line.len());
    let (mut start, mut end) = (0, line.len());
    if line.len() > MAX_TEXT_BYTES {
        start = found
            .start
            .saturating_sub(BEFORE_MATCH)
            .min(line.len() - MAX_TEXT_BYTES);
        end = start + MAX_TEXT_BYTES;
        // Never cut a letter in half. The match starts a letter, so moving right keeps it.
        if let Some(letter) = letter_around(line, start) {
            start = letter.end.min(found.start);
        }
        if let Some(letter) = letter_around(line, end) {
            end = letter.start.max(start);
        }
    }
    let text = String::from_utf8_lossy(&line[start..end]);
    let shown = found.start.clamp(start, end)..found.end.clamp(start, end);
    let before = match &text {
        Cow::Borrowed(_) => shown.start - start,
        Cow::Owned(_) => lossy_len(&line[start..shown.start]),
    };
    (text, before..before + shown.len())
}

/// The first match in `line` that is still there once it is trimmed to `kept`, in trimmed
/// places: typed text can begin or end with spaces that the shown line drops.
fn shown_match(matcher: &Literal, line: &[u8], kept: Range<usize>) -> Range<usize> {
    let mut at = 0;
    while let Ok(Some(found)) = matcher.find_at(line, at) {
        let start = found.start().clamp(kept.start, kept.end);
        let end = found.end().clamp(kept.start, kept.end);
        if start < end {
            return start - kept.start..end - kept.start;
        }
        if found.start() >= kept.end {
            break;
        }
        at = found.start() + 1;
    }
    0..0
}

/// The length once each broken sequence becomes one stand-in letter, as `from_utf8_lossy`
/// does it: a run of broken bytes can hold several sequences.
fn lossy_len(bytes: &[u8]) -> usize {
    bytes
        .utf8_chunks()
        .map(|chunk| {
            let replaced = !chunk.invalid().is_empty();
            chunk.valid().len() + usize::from(replaced) * char::REPLACEMENT_CHARACTER.len_utf8()
        })
        .sum()
}

/// The letter that a cut at `at` would split. Stray bytes split nothing, so a run of them
/// never drags the cut into the match.
fn letter_around(line: &[u8], at: usize) -> Option<Range<usize>> {
    if !line.get(at).is_some_and(|&byte| is_inside_letter(byte)) {
        return None;
    }
    let lead = (at.saturating_sub(3)..at)
        .rev()
        .find(|&i| !is_inside_letter(line[i]))?;
    let len = match line[lead] {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return None,
    };
    (lead + len > at).then_some(lead..lead + len)
}

/// A UTF-8 byte that continues a letter rather than starting one.
fn is_inside_letter(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

#[cfg(test)]
mod tests;
