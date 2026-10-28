use std::ffi::OsString;
use std::io::BufRead;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Instant;
use telescope_core::{
    Access, Apps, Boosts, Clutter, Frecency, Hit, Ignore, Index, Item, Opened, Searcher, Settings,
    unix_now,
};
use telescope_grep::Grep;

#[global_allocator]
static PAGES: telescope_core::Pages = telescope_core::Pages;

const USAGE: &str = "usage:
  tl index <folder>                            scan and report the index size
  tl search <folder>                           scan, then search each line you type
  tl bench <folder> <queries> [threads]        time every keystroke of each query
  tl golden <folder> <golden>                  rank the expected file of each search
  tl grep <folder> <text>                      scan, then show lines holding the text
  tl grep-bench <folder> <queries> [threads]   time the first and the last line of each text
  tl open-record <path> --frecency <file>      count one open of a file or folder in a list
on a Mac, searches also find the apps in /Applications, /System/Applications and ~/Applications
add --all to include hidden folders, ~/Library, the insides of packages like Photos Library,
and tool downloads and caches like node_modules
add --load <file> to open a saved index instead of scanning (it scans when the file is missing,
damaged or built with other settings, and does not catch up on changes made since it was saved),
--changes <file> to then read again each folder listed in it, one real absolute path per line
as FSEvents reports them, and --save <file> to save the index afterwards
add --frecency <file> to search, bench and golden to rank files opened often and lately higher,
by the opens tl open-record kept in it, a list apart from the Mac app's, which writes over its
own from memory
put -- before text that starts with --, like: tl grep ~ -- --all

your own ignore list, ~/.config/telescope/ignore, takes one folder per line
(--all leaves it out too):
  Old Projects        skip every folder with this name
  ~/Movies/Exports    skip this one folder
  !~/.config/nvim     bring back a folder Telescope skips, even inside a skipped one
  # a comment";

/// Lines `tl grep` shows, as many as the window would.
const GREP_LIMIT: usize = 100;
/// Far more than any Mac has cores; a count in the millions would fail to start the threads.
const MAX_THREADS: usize = 256;

#[derive(Debug, PartialEq)]
struct Options {
    clutter: Clutter,
    load: Option<PathBuf>,
    changes: Option<PathBuf>,
    save: Option<PathBuf>,
    frecency: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
enum Command {
    Version,
    Index(PathBuf),
    Search(PathBuf),
    Bench {
        folder: PathBuf,
        queries: PathBuf,
        threads: Option<usize>,
    },
    Golden {
        folder: PathBuf,
        cases: PathBuf,
    },
    Grep {
        folder: PathBuf,
        text: String,
    },
    GrepBench {
        folder: PathBuf,
        queries: PathBuf,
        threads: Option<usize>,
    },
    OpenRecord {
        path: PathBuf,
        list: PathBuf,
    },
}

fn main() -> ExitCode {
    // Piped into `head`, stop quietly like other command-line tools instead of panicking.
    // SAFETY: nothing else runs yet, and the default action is a valid one for SIGPIPE.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let Some((command, options)) = parse(std::env::args_os().skip(1).collect()) else {
        return usage();
    };
    let options = &options;
    match command {
        Command::Version => {
            println!("tl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Index(folder) => match load(&folder, options) {
            Some(_) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        },
        Command::Search(folder) => search(&folder, options),
        Command::Bench {
            folder,
            queries,
            threads,
        } => bench(&folder, options, &queries, threads),
        Command::Golden { folder, cases } => golden(&folder, options, &cases),
        Command::Grep { folder, text } => grep(&folder, options, &text),
        Command::GrepBench {
            folder,
            queries,
            threads,
        } => grep_bench(&folder, options, &queries, threads),
        Command::OpenRecord { path, list } => open_record(&path, &list),
    }
}

/// `None` when the line does not fit any command, so usage is shown.
fn parse(mut args: Vec<OsString>) -> Option<(Command, Options)> {
    let after_dashes: Vec<OsString> = match args.iter().position(|arg| arg == "--") {
        Some(at) => args.drain(at..).skip(1).collect(),
        None => Vec::new(),
    };
    let clutter = if args.iter().any(|arg| arg == "--all") {
        Clutter::Keep
    } else {
        Clutter::Skip
    };
    args.retain(|arg| arg != "--all");
    let load = take(&mut args, "--load")?;
    let changes = take(&mut args, "--changes")?;
    let save = take(&mut args, "--save")?;
    let frecency = take(&mut args, "--frecency")?;
    args.extend(after_dashes);
    let command = match args.as_slice() {
        [flag] if flag == "--version" => Command::Version,
        [command, folder] if command == "index" => Command::Index(folder.into()),
        [command, folder] if command == "search" => Command::Search(folder.into()),
        [command, folder, queries, rest @ ..] if command == "bench" => Command::Bench {
            folder: folder.into(),
            queries: queries.into(),
            threads: threads(rest)?,
        },
        [command, folder, cases] if command == "golden" => Command::Golden {
            folder: folder.into(),
            cases: cases.into(),
        },
        [command, folder, text] if command == "grep" => Command::Grep {
            folder: folder.into(),
            // Empty text, or text over two lines, would find nothing after a whole scan.
            text: text
                .to_str()
                .filter(|text| !text.is_empty() && !text.contains('\n'))?
                .to_owned(),
        },
        [command, folder, queries, rest @ ..] if command == "grep-bench" => Command::GrepBench {
            folder: folder.into(),
            queries: queries.into(),
            threads: threads(rest)?,
        },
        [command, path] if command == "open-record" => Command::OpenRecord {
            path: path.into(),
            list: frecency.clone()?,
        },
        _ => return None,
    };
    Some((
        command,
        Options {
            clutter,
            load,
            changes,
            save,
            frecency,
        },
    ))
}

/// The optional thread count after a bench's query file. Zero would mean every core to rayon,
/// so it is refused rather than read as that.
fn threads(rest: &[OsString]) -> Option<Option<usize>> {
    match rest {
        [] => Some(None),
        [threads] => {
            let threads = threads.to_str()?.parse::<NonZero<usize>>().ok()?.get();
            (threads <= MAX_THREADS).then_some(Some(threads))
        }
        _ => None,
    }
}

/// Removes `flag` and the file after it. `None` when the file is missing or the flag comes
/// twice.
fn take(args: &mut Vec<OsString>, flag: &str) -> Option<Option<PathBuf>> {
    let Some(at) = args.iter().position(|arg| arg == flag) else {
        return Some(None);
    };
    let file = args.get(at + 1)?;
    if file.as_encoded_bytes().starts_with(b"--") {
        return None;
    }
    let file = PathBuf::from(file);
    args.drain(at..=at + 1);
    (!args.iter().any(|arg| arg == flag)).then_some(Some(file))
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::FAILURE
}

fn ignore_list(home: &Path) -> Ignore {
    let (ignore, problems) = Ignore::load(home, Access::Full);
    for problem in problems {
        eprintln!("tl: {problem}");
    }
    ignore
}

fn load(folder: &Path, options: &Options) -> Option<Index> {
    let clutter = options.clutter;
    let started = Instant::now();
    let home = telescope_core::home();
    let ignore = match clutter {
        Clutter::Skip => ignore_list(&home),
        Clutter::Keep => Ignore::default(),
    };
    let settings = Settings::new(folder, &home, clutter, ignore, Access::Full);
    // The command line has no FSEvents of its own, so it keeps the event an opened index had.
    let mut opened_at = None;
    let scan = match &options.load {
        None => settings.scan(),
        Some(file) => telescope_core::open_or_scan(file, &settings).map(|found| match found {
            Opened::Saved(saved) => {
                opened_at = Some(saved.event_id);
                saved.scan
            }
            Opened::Scanned { scan, why } => {
                eprintln!("tl: cannot open {}: {why}; scanned instead", file.display());
                scan
            }
        }),
    };
    let took = ms(started);
    let mut scan = match scan {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!("tl: cannot scan {}: {error}", folder.display());
            return None;
        }
    };
    if opened_at.is_some() {
        println!("open time: {took:.1} ms");
    } else {
        println!("scan time: {:.2} s", took / 1000.0);
    }
    if let Some(file) = &options.changes {
        apply_changes(file, &settings, &mut scan.index)?;
    }

    let index = &scan.index;
    println!(
        "{} entries ({} folders), {} distinct names",
        index.entry_count(),
        index.folder_count(),
        index.distinct_names()
    );
    println!("index memory: {:.1} MB", mb(index.heap_bytes() as u64));
    if scan.unreadable_folders > 0 {
        println!("folders not fully read: {}", scan.unreadable_folders);
    }
    match &options.save {
        Some(file) if unchanged_since_opened(options, opened_at.is_some()) => {
            println!("opened unchanged from {}, not saved again", file.display());
        }
        Some(file) => {
            let started = Instant::now();
            match telescope_core::save_index(file, &scan, &settings, opened_at.unwrap_or(0)) {
                Ok(()) => {
                    let size = std::fs::metadata(file).map_or(0, |file| file.len());
                    println!("saved {:.1} MB in {:.1} ms", mb(size), ms(started));
                }
                Err(error) => eprintln!("tl: cannot save {}: {error}", file.display()),
            }
        }
        None => {}
    }
    Some(scan.index)
}

fn apply_changes(file: &Path, settings: &Settings, index: &mut Index) -> Option<()> {
    let text = read(file)?;
    let changed = changed_paths(&text);
    let root = settings.root();
    let outside = changed
        .iter()
        .filter(|path| !path.starts_with(root))
        .count();
    if outside > 0 {
        eprintln!(
            "tl: skipped {outside} paths not under {}; the list takes real absolute paths",
            root.display()
        );
    }
    let started = Instant::now();
    let update = settings.update(index, &changed);
    println!(
        "read {} changed folders again in {:.1} ms: {} entries added, {} removed{}",
        update.folders_read,
        ms(started),
        update.added,
        update.removed,
        if update.compacted { ", compacted" } else { "" }
    );
    if update.unreadable_folders > 0 {
        println!(
            "folders not fully read in this update: {}",
            update.unreadable_folders
        );
    }
    Some(())
}

/// Blank lines are skipped; `lines` already takes off Windows line ends.
fn changed_paths(text: &str) -> Vec<&Path> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(Path::new)
        .collect()
}

fn unchanged_since_opened(options: &Options, opened: bool) -> bool {
    opened && options.changes.is_none() && options.load == options.save
}

/// The apps a window would show beside the files, on a Mac.
fn apps() -> Apps {
    let started = Instant::now();
    let apps = Apps::scan(&telescope_core::home());
    println!(
        "{} apps read in {:.1} ms, {:.1} KB",
        apps.len(),
        ms(started),
        apps.heap_bytes() as f64 / 1024.0
    );
    apps
}

fn path(index: &Index, apps: &Apps, hit: &Hit) -> PathBuf {
    match hit.item {
        Item::Entry(entry) => index.path(entry),
        Item::App(app) => apps.path(app),
    }
}

/// Where the expected path sits among the hits. An app's path may be a link, like Safari's,
/// and the expected one is real.
fn rank(index: &Index, apps: &Apps, hits: &[Hit], expected: &Path) -> Option<usize> {
    hits.iter().position(|hit| {
        let path = path(index, apps, hit);
        path == expected
            || (matches!(hit.item, Item::App(_))
                && std::fs::canonicalize(&path).is_ok_and(|real| real == expected))
    })
}

fn search(folder: &Path, options: &Options) -> ExitCode {
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let apps = apps();
    let boosts = boosts(&index, &apps, options);
    let searcher = Searcher::default();
    eprintln!("type a search and press Enter; Ctrl-D quits");
    for line in std::io::stdin().lock().lines() {
        let Ok(query) = line else { break };
        let started = Instant::now();
        let hits = searcher
            .search_with(&index, &apps, &query, searcher.ticket(), 10, &boosts)
            .unwrap_or_default();
        println!("{query:?} in {:.1} ms", ms(started));
        for hit in hits {
            println!("{:>8}  {}", hit.score, path(&index, &apps, &hit).display());
        }
    }
    ExitCode::SUCCESS
}

fn boosts(index: &Index, apps: &Apps, options: &Options) -> Boosts {
    let Some(file) = &options.frecency else {
        return Boosts::default();
    };
    let frecency = Frecency::open(file).unwrap_or_else(|why| {
        eprintln!("tl: cannot open {}: {why}", file.display());
        Frecency::default()
    });
    let started = Instant::now();
    let boosts = frecency.boosts(index, apps, unix_now());
    println!(
        "frecency: {} of {} files opened are in the index or the apps, found in {:.1} ms",
        boosts.len(),
        frecency.len(),
        ms(started)
    );
    boosts
}

/// The path as the scan lists it: its folders resolved, but a link itself, not where it points.
fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::symlink_metadata(path)?;
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            Ok(std::fs::canonicalize(parent)?.join(name))
        }
        _ => std::fs::canonicalize(path),
    }
}

fn open_record(path: &Path, file: &Path) -> ExitCode {
    let path = match real_path(path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("tl: cannot find {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut frecency = match Frecency::open(file) {
        Ok(frecency) => frecency,
        Err(why) if why.is_damaged() => {
            eprintln!(
                "tl: cannot open {}: {why}; starting a new list",
                file.display()
            );
            Frecency::default()
        }
        Err(why) => {
            eprintln!("tl: cannot open {}: {why}; left as it is", file.display());
            return ExitCode::FAILURE;
        }
    };
    let now = unix_now();
    if !frecency.record(&path, now) {
        eprintln!(
            "tl: cannot count {}: not UTF-8, or longer than 4096 bytes",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    if let Err(error) = frecency.save(file) {
        eprintln!("tl: cannot save {}: {error}", file.display());
        return ExitCode::FAILURE;
    }
    println!(
        "{}: score {:.2}, one of {} files opened in {}",
        path.display(),
        frecency.score(&path, now),
        frecency.len(),
        file.display()
    );
    ExitCode::SUCCESS
}

fn read(file: &Path) -> Option<String> {
    std::fs::read_to_string(file)
        .inspect_err(|error| eprintln!("tl: cannot read {}: {error}", file.display()))
        .ok()
}

/// One query per line, up to a tab; lines starting with `#` are comments.
fn queries(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').next().unwrap_or(line))
        .filter(|query| !query.trim().is_empty())
        .collect()
}

/// The item below which `share` percent of the non-empty `sorted` fall. With 20 items or fewer,
/// p95 is the last one.
fn percentile<T>(sorted: &[T], share: usize) -> &T {
    &sorted[(sorted.len() * share / 100).min(sorted.len() - 1)]
}

fn bench(folder: &Path, options: &Options, query_file: &Path, threads: Option<usize>) -> ExitCode {
    let Some(text) = read(query_file) else {
        return ExitCode::FAILURE;
    };
    let queries = queries(&text);
    if queries.is_empty() {
        eprintln!("tl: no queries in {}", query_file.display());
        return ExitCode::FAILURE;
    }
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let apps = apps();
    let boosts = boosts(&index, &apps, options);
    let searcher = threads.map_or_else(Searcher::default, Searcher::with_threads);

    let mut times: Vec<(f64, &str)> = Vec::new();
    for query in &queries {
        for typed in keystrokes(query) {
            let started = Instant::now();
            searcher.search_with(&index, &apps, typed, searcher.ticket(), 50, &boosts);
            times.push((ms(started), typed));
        }
    }
    times.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at = |share| percentile(&times, share).0;
    println!(
        "{} keystrokes from {} queries on {} search threads",
        times.len(),
        queries.len(),
        searcher.threads()
    );
    println!(
        "p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        at(50),
        at(95),
        at(100)
    );
    for (time, typed) in times.iter().rev().take(3) {
        println!("  slowest: {typed:?} {time:.1} ms");
    }

    // A burst is 8 keystrokes typed faster than one search. Only the last should be shown,
    // and the others should give up early, so a whole burst costs about one search.
    let mut most_delivered = 0;
    let mut slowest_burst = 0.0f64;
    for query in &queries {
        let typed = keystrokes(query);
        let started = Instant::now();
        let delivered = std::thread::scope(|scope| {
            let running: Vec<_> = (0..8)
                .map(|i| {
                    let query = typed[i.min(typed.len() - 1)];
                    let ticket = searcher.ticket();
                    let (searcher, index, apps, boosts) = (&searcher, &index, &apps, &boosts);
                    scope.spawn(move || {
                        let hits = searcher.search_with(index, apps, query, ticket, 50, boosts);
                        hits.is_some()
                    })
                })
                .collect();
            running
                .into_iter()
                .map(|search| search.join().unwrap())
                .filter(|&delivered| delivered)
                .count()
        });
        slowest_burst = slowest_burst.max(ms(started));
        most_delivered = most_delivered.max(delivered);
    }
    println!(
        "bursts of 8 keystrokes: at most {most_delivered} results delivered, slowest burst {slowest_burst:.1} ms"
    );
    ExitCode::SUCCESS
}

/// Each line is a search, a tab, and the file you expect near the top.
fn golden(folder: &Path, options: &Options, case_file: &Path) -> ExitCode {
    let Some(text) = read(case_file) else {
        return ExitCode::FAILURE;
    };
    let home = std::env::home_dir().unwrap_or_default();
    let cases: Vec<(&str, PathBuf)> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('\t'))
        .map(|(query, expected)| {
            let expected = match expected.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(expected),
            };
            // The index holds real paths, as the scan resolved them.
            (query, std::fs::canonicalize(&expected).unwrap_or(expected))
        })
        .collect();
    if cases.is_empty() {
        eprintln!("tl: no searches in {}", case_file.display());
        return ExitCode::FAILURE;
    }
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let apps = apps();
    let boosts = boosts(&index, &apps, options);
    let searcher = Searcher::default();

    let mut in_top_3 = 0;
    for (query, expected) in &cases {
        let hits = searcher
            .search_with(&index, &apps, query, searcher.ticket(), 100, &boosts)
            .unwrap_or_default();
        let rank = rank(&index, &apps, &hits, expected);
        let place = match rank {
            Some(rank) => (rank + 1).to_string(),
            None if expected.exists() => "-".to_string(),
            None => "gone".to_string(),
        };
        println!("{place:>4}  {query}");
        if rank.is_some_and(|rank| rank < 3) {
            in_top_3 += 1;
        } else {
            for hit in hits.iter().take(3) {
                println!("{:>10}  {}", hit.score, path(&index, &apps, hit).display());
            }
        }
    }
    println!("expected file in the top 3: {in_top_3} of {}", cases.len());
    ExitCode::SUCCESS
}

fn grep(folder: &Path, options: &Options, text: &str) -> ExitCode {
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let grep = Grep::default();
    let started = Instant::now();
    let first = OnceLock::new();
    let report = grep.search(&index, text, grep.ticket(), GREP_LIMIT, |found| {
        first.get_or_init(|| ms(started));
        println!("{}:{}: {}", found.path.display(), found.line, found.text);
    });
    let Some(report) = report else {
        return ExitCode::FAILURE;
    };
    let first = first.into_inner().map_or_else(String::new, |first| {
        format!(", the first after {first:.1} ms")
    });
    println!(
        "{} lines in {} files searched in {:.1} ms on {} threads{first}",
        report.lines_found,
        report.files_searched,
        ms(started),
        grep.threads(),
    );
    println!(
        "skipped: {} binary, {} too large, {} cloud-only, {} unreadable",
        report.binary, report.too_large, report.cloud_only, report.unreadable
    );
    ExitCode::SUCCESS
}

fn grep_bench(
    folder: &Path,
    options: &Options,
    query_file: &Path,
    threads: Option<usize>,
) -> ExitCode {
    let Some(text) = read(query_file) else {
        return ExitCode::FAILURE;
    };
    let queries = queries(&text);
    if queries.is_empty() {
        eprintln!("tl: no queries in {}", query_file.display());
        return ExitCode::FAILURE;
    }
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let loaded = memory();
    let grep = threads.map_or_else(Grep::default, Grep::with_threads);

    let mut firsts = Vec::with_capacity(queries.len());
    let mut totals = Vec::with_capacity(queries.len());
    for query in &queries {
        let first = OnceLock::new();
        let started = Instant::now();
        let report = grep.search(&index, query, grep.ticket(), GREP_LIMIT, |_| {
            first.get_or_init(|| ms(started));
        });
        let total = ms(started);
        let report = report.unwrap_or_default();
        let first = first.into_inner();
        println!(
            "{query:?}: {} lines in {} files, first {}, all {total:.1} ms",
            report.lines_found,
            report.files_searched,
            first.map_or_else(|| "none".to_string(), |first| format!("{first:.1} ms")),
        );
        firsts.extend(first);
        totals.push((total, *query));
    }

    println!(
        "{} searches on {} threads, up to {GREP_LIMIT} lines each",
        queries.len(),
        grep.threads()
    );
    firsts.sort_by(f64::total_cmp);
    if firsts.is_empty() {
        println!("first line: no search found any");
    } else {
        println!(
            "first line: p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms ({} of {} searches found lines)",
            percentile(&firsts, 50),
            percentile(&firsts, 95),
            percentile(&firsts, 100),
            firsts.len(),
            queries.len()
        );
    }
    totals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at = |share| percentile(&totals, share).0;
    println!(
        "all lines:  p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        at(50),
        at(95),
        at(100)
    );
    for (time, query) in totals.iter().rev().take(3) {
        println!("  slowest: {query:?} {time:.1} ms");
    }
    if let (Some(loaded), Some(after)) = (loaded, memory()) {
        let when = if after.peak > loaded.peak {
            "during the searches"
        } else {
            "while loading the index, which the searches stayed under"
        };
        println!(
            "memory: {:.1} MB with the index loaded, peak {:.1} MB {when}",
            mb(loaded.now),
            mb(after.peak)
        );
    }
    ExitCode::SUCCESS
}

/// Footprint, as the `footprint` tool and Activity Monitor count it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Memory {
    now: u64,
    /// The most this process has held since it started.
    peak: u64,
}

#[cfg(target_os = "macos")]
fn memory() -> Option<Memory> {
    let pid = i32::try_from(std::process::id()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
    // SAFETY: RUSAGE_INFO_V4 makes the call write one whole rusage_info_v4, which `info` has
    // room for, and `info` is read only when the call reports success.
    unsafe {
        (libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) == 0).then(
            || {
                let info = info.assume_init();
                Memory {
                    now: info.ri_phys_footprint,
                    peak: info.ri_lifetime_max_phys_footprint,
                }
            },
        )
    }
}

#[cfg(not(target_os = "macos"))]
fn memory() -> Option<Memory> {
    None
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// What the search box holds after each keystroke: "b", "bu", "bul", ...
fn keystrokes(query: &str) -> Vec<&str> {
    let mut typed: Vec<&str> = query
        .char_indices()
        .map(|(i, c)| query[..i + c.len_utf8()].trim_end())
        .filter(|typed| !typed.is_empty())
        .collect();
    typed.dedup();
    typed
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<OsString> {
        line.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn takes_a_flag_and_the_file_after_it() {
        let mut line = args("index ~ --load a.idx --save b.idx");
        assert_eq!(take(&mut line, "--load"), Some(Some("a.idx".into())));
        assert_eq!(line, args("index ~ --save b.idx"));
        assert_eq!(take(&mut line, "--save"), Some(Some("b.idx".into())));
        assert_eq!(line, args("index ~"));
        assert_eq!(take(&mut line, "--save"), Some(None));
        assert_eq!(line, args("index ~"));
    }

    #[test]
    fn refuses_a_flag_with_no_file() {
        let mut line = args("index ~ --load");
        assert_eq!(take(&mut line, "--load"), None);
        let mut line = args("index ~ --load --save out.idx");
        assert_eq!(take(&mut line, "--load"), None);
        assert_eq!(line, args("index ~ --load --save out.idx"));
    }

    #[test]
    fn refuses_a_flag_given_twice() {
        let mut line = args("index ~ --load a --load b");
        assert_eq!(take(&mut line, "--load"), None);
        assert_eq!(command("index ~ --load a --load b"), None);
        assert_eq!(command("grep ~ --load a.idx --load"), None);
        assert_eq!(command("grep ~ todo --save a --save b"), None);
        assert_eq!(command("index ~ --changes a --changes b"), None);
    }

    fn command(line: &str) -> Option<Command> {
        parse(args(line)).map(|(command, _)| command)
    }

    fn grep_bench_on(threads: Option<usize>) -> Command {
        Command::GrepBench {
            folder: "~".into(),
            queries: "q.txt".into(),
            threads,
        }
    }

    #[test]
    fn reads_each_command() {
        assert_eq!(command("--version"), Some(Command::Version));
        assert_eq!(command("index ~"), Some(Command::Index("~".into())));
        assert_eq!(command("search ~"), Some(Command::Search("~".into())));
        let bench_on = |threads| Command::Bench {
            folder: "~".into(),
            queries: "q.txt".into(),
            threads,
        };
        assert_eq!(command("bench ~ q.txt"), Some(bench_on(None)));
        assert_eq!(command("bench ~ q.txt 4"), Some(bench_on(Some(4))));
        assert_eq!(command("bench ~ q.txt +4"), Some(bench_on(Some(4))));
        assert_eq!(
            command("bench ~ q.txt 256"),
            Some(bench_on(Some(MAX_THREADS)))
        );
        assert_eq!(
            command("golden ~ g.tsv"),
            Some(Command::Golden {
                folder: "~".into(),
                cases: "g.tsv".into(),
            })
        );
        assert_eq!(
            command("grep ~ todo"),
            Some(Command::Grep {
                folder: "~".into(),
                text: "todo".into(),
            })
        );
        assert_eq!(command("grep-bench ~ q.txt"), Some(grep_bench_on(None)));
        assert_eq!(
            command("grep-bench ~ q.txt 3"),
            Some(grep_bench_on(Some(3)))
        );
        assert_eq!(
            command("open-record ~/a.pdf --frecency f"),
            Some(Command::OpenRecord {
                path: "~/a.pdf".into(),
                list: "f".into()
            })
        );
    }

    #[test]
    fn keeps_the_text_to_find_whole() {
        let line = ["grep", "~", "  fn main() {"].map(OsString::from).to_vec();
        let (found, _) = parse(line).unwrap();
        assert_eq!(
            found,
            Command::Grep {
                folder: "~".into(),
                text: "  fn main() {".into(),
            }
        );
        let grep_for = |text: &str| {
            Some(Command::Grep {
                folder: "~".into(),
                text: text.into(),
            })
        };
        assert_eq!(command("grep ~ --version"), grep_for("--version"));
        for flag in ["--all", "--load", "--changes", "--save", "--frecency", "--"] {
            assert_eq!(command(&format!("grep ~ -- {flag}")), grep_for(flag));
        }
        assert_eq!(command("grep -- ~ --all"), grep_for("--all"));
        assert_eq!(command("grep ~ --load a.idx -- --load"), grep_for("--load"));
        let (_, options) = parse(args("--all grep ~ -- --all")).unwrap();
        assert_eq!(options.clutter, Clutter::Keep);
        // A flag after the dashes is text, so this line has one word too many.
        assert_eq!(command("grep ~ todo -- --all"), None);
        assert_eq!(command("grep ~ --all"), None);
    }

    #[test]
    fn refuses_text_that_can_never_be_found() {
        for text in ["", "a\nb", "\n"] {
            let line = ["grep", "~", text].map(OsString::from).to_vec();
            assert!(parse(line).is_none(), "{text:?}");
        }
        let line = ["grep", "~", "a\rb"].map(OsString::from).to_vec();
        assert!(parse(line).is_some());
    }

    #[test]
    fn refuses_lines_that_fit_no_command() {
        for line in [
            "",
            "--version extra",
            "index",
            "index ~ extra",
            "search",
            "search ~ x",
            "golden ~",
            "golden ~ g.tsv x",
            "bench ~",
            "bench ~ q.txt 4 5",
            "bench ~ q.txt 257",
            "bench ~ q.txt 1000000",
            "bench ~ q.txt 99999999999999999999999",
            "find ~",
            "grep ~",
            "grep ~ two words",
            "grep-bench ~",
            "grep-bench ~ q.txt 0",
            "grep-bench ~ q.txt -1",
            "grep-bench ~ q.txt four",
            "grep-bench ~ q.txt 4 5",
            "bench ~ q.txt 0",
            "grep ~ todo --load",
            "GREP ~ todo",
            "open-record",
            "open-record a",
            "open-record a b --frecency f",
            "open-record a --frecency",
            "golden ~ g.tsv --frecency a --frecency b",
        ] {
            assert_eq!(command(line), None, "{line:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_text_and_thread_counts_that_are_not_utf8_but_keeps_such_folders() {
        use std::os::unix::ffi::OsStringExt;
        let broken = || OsString::from_vec(vec![0xFF]);
        let line = vec!["grep".into(), "~".into(), broken()];
        assert!(parse(line).is_none());
        let line = vec!["bench".into(), "~".into(), "q.txt".into(), broken()];
        assert!(parse(line).is_none());
        let line = vec!["index".into(), broken()];
        assert_eq!(parse(line).unwrap().0, Command::Index(broken().into()));
    }

    #[test]
    fn reads_the_flags_anywhere_on_the_line() {
        let (found, options) = parse(args("--all grep-bench ~ --load a.idx q.txt 2")).unwrap();
        assert_eq!(found, grep_bench_on(Some(2)));
        assert_eq!(
            options,
            Options {
                clutter: Clutter::Keep,
                load: Some("a.idx".into()),
                changes: None,
                save: None,
                frecency: None,
            }
        );
        let line = "grep ~ todo --save b.idx --changes c.txt --frecency f";
        let (_, options) = parse(args(line)).unwrap();
        assert_eq!(
            options,
            Options {
                clutter: Clutter::Skip,
                load: None,
                changes: Some("c.txt".into()),
                save: Some("b.idx".into()),
                frecency: Some("f".into()),
            }
        );
        let (_, options) = parse(args("index ~ --load a.idx --save a.idx")).unwrap();
        assert_eq!(options.load, Some("a.idx".into()));
        assert_eq!(options.save, Some("a.idx".into()));
    }

    #[test]
    fn reads_one_query_per_line() {
        let text = "# a comment\nreadme\n\n   \nfn main\t~/src/main.rs\n\tonly a file\n #not a comment\r\nlast";
        assert_eq!(
            queries(text),
            ["readme", "fn main", " #not a comment", "last"]
        );
        assert!(queries("").is_empty());
        assert!(queries("# only\n# comments\n").is_empty());
        // Searched exactly as written, so spaces and a stray carriage return stay.
        assert_eq!(queries("todo \nlast\r"), ["todo ", "last\r"]);
    }

    #[test]
    fn every_query_is_one_whole_line_of_text() {
        let pieces = ["a", " ", "\t", "\n", "\r\n", "#", "b c"];
        // Every 4-piece line built from the pieces, 7^4 of them.
        for mut n in 0..pieces.len().pow(4) {
            let mut text = String::new();
            for _ in 0..4 {
                text.push_str(pieces[n % pieces.len()]);
                n /= pieces.len();
            }
            for query in queries(&text) {
                assert!(!query.contains(['\t', '\n']), "{text:?} gave {query:?}");
                assert!(!query.starts_with('#'), "{text:?} gave {query:?}");
                assert!(!query.trim().is_empty(), "{text:?} gave {query:?}");
            }
        }
    }

    #[test]
    fn takes_percentiles_from_sorted_times() {
        assert_eq!(*percentile(&[7.0], 50), 7.0);
        assert_eq!(*percentile(&[7.0], 100), 7.0);
        let times: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(*percentile(&times, 0), 1.0);
        assert_eq!(*percentile(&times, 50), 11.0);
        assert_eq!(*percentile(&times, 100), 20.0);
        let times: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(*percentile(&times, 95), 96.0);
    }

    #[test]
    fn p95_is_the_slowest_with_20_times_or_fewer() {
        for len in 1..=50 {
            let times: Vec<usize> = (0..len).collect();
            assert_eq!(*percentile(&times, 0), 0);
            assert_eq!(*percentile(&times, 100), len - 1);
            assert_eq!(*percentile(&times, 95) == len - 1, len <= 20, "{len}");
            for share in 1..=100 {
                assert!(percentile(&times, share) >= percentile(&times, share - 1));
            }
        }
    }

    #[test]
    fn bench_and_grep_bench_count_percentiles_alike() {
        let times: Vec<(f64, &str)> = (1..=30).map(|n| (f64::from(n), "q")).collect();
        let alone: Vec<f64> = times.iter().map(|(time, _)| *time).collect();
        for share in [50, 95, 100] {
            assert_eq!(percentile(&times, share).0, *percentile(&alone, share));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn memory_counts_what_is_held_and_keeps_the_peak() {
        let before = memory().unwrap();
        let held = std::hint::black_box(vec![1u8; 64 << 20]);
        let during = memory().unwrap();
        assert!(
            during.now >= before.now + (60 << 20),
            "{} {}",
            before.now,
            during.now
        );
        assert!(during.peak >= during.now);
        drop(held);
        let after = memory().unwrap();
        assert!(after.peak >= during.now, "{} {}", during.now, after.peak);
    }

    #[test]
    fn takes_one_changed_path_per_line_and_skips_blank_ones() {
        let text = "/home/a b\r\n\r\n/home/c\n\n/home/d";
        let expected = ["/home/a b", "/home/c", "/home/d"].map(Path::new);
        assert_eq!(changed_paths(text), expected);
        assert!(changed_paths("").is_empty());
    }

    #[test]
    fn saves_again_unless_opened_from_the_same_file_with_no_changes() {
        let options = |load: Option<&str>, changes: Option<&str>, save: Option<&str>| Options {
            clutter: Clutter::Skip,
            load: load.map(PathBuf::from),
            changes: changes.map(PathBuf::from),
            save: save.map(PathBuf::from),
            frecency: None,
        };
        let same = options(Some("i"), None, Some("i"));
        assert!(unchanged_since_opened(&same, true));
        // Scanned because the file could not be used.
        assert!(!unchanged_since_opened(&same, false));
        assert!(!unchanged_since_opened(
            &options(Some("i"), Some("c"), Some("i")),
            true
        ));
        assert!(!unchanged_since_opened(
            &options(Some("i"), None, Some("j")),
            true
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ranks_an_app_reached_through_a_link_by_its_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for folder in ["sealed/Safari.app", "Apps", "files"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("sealed/Safari.app"), root.join("Apps/Safari.app"))
            .unwrap();
        std::fs::write(root.join("files/safari notes.txt"), "").unwrap();
        let apps = Apps::scan_folders(vec![root.join("Apps")]);
        let files = root.join("files");
        let ignore = Ignore::default();
        let index = telescope_core::scan(&files, &files, Clutter::Skip, &ignore, Access::Full)
            .unwrap()
            .index;
        let searcher = Searcher::with_threads(1);
        let hits = searcher
            .search(&index, &apps, "safari", searcher.ticket(), 10)
            .unwrap();
        let at = |expected: &str| rank(&index, &apps, &hits, &root.join(expected));
        assert_eq!(at("sealed/Safari.app"), Some(0));
        assert_eq!(at("Apps/Safari.app"), Some(0));
        assert_eq!(at("files/safari notes.txt"), Some(1));
        assert_eq!(at("files/other.txt"), None);
    }
}
