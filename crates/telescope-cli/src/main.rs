use std::ffi::OsString;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;
use telescope_core::{Access, Clutter, Ignore, Index, Opened, Searcher, Settings};
use telescope_grep::Grep;

const USAGE: &str = "usage:
  tl index <folder>                       scan and report the index size
  tl search <folder>                      scan, then search each line you type
  tl bench <folder> <queries> [threads]   time every keystroke of each query
  tl golden <folder> <golden>             rank the expected file of each search
  tl grep <folder> <text>                 scan, then show lines holding the text
add --all to include hidden folders, ~/Library, the insides of packages like Photos Library,
and tool downloads and caches like node_modules
add --load <file> to open a saved index instead of scanning (it scans when the file is missing,
damaged or built with other settings, and does not catch up on changes made since it was saved),
--changes <file> to then read again each folder listed in it, one real absolute path per line
as FSEvents reports them, and --save <file> to save the index afterwards

your own ignore list, ~/.config/telescope/ignore, takes one folder per line
(--all leaves it out too):
  Old Projects        skip every folder with this name
  ~/Movies/Exports    skip this one folder
  !~/.config/nvim     bring back a folder Telescope skips, even inside a skipped one
  # a comment";

struct Options {
    clutter: Clutter,
    load: Option<PathBuf>,
    changes: Option<PathBuf>,
    save: Option<PathBuf>,
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let clutter = if args.iter().any(|arg| arg == "--all") {
        Clutter::Keep
    } else {
        Clutter::Skip
    };
    args.retain(|arg| arg != "--all");
    let (Some(load_file), Some(changes), Some(save_file)) = (
        take(&mut args, "--load"),
        take(&mut args, "--changes"),
        take(&mut args, "--save"),
    ) else {
        return usage();
    };
    let options = &Options {
        clutter,
        load: load_file,
        changes,
        save: save_file,
    };
    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("tl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [command, folder] if command == "index" => match load(Path::new(folder), options) {
            Some(_) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        },
        [command, folder] if command == "search" => search(Path::new(folder), options),
        [command, folder, queries] if command == "bench" => {
            bench(Path::new(folder), options, Path::new(queries), None)
        }
        [command, folder, queries, threads] if command == "bench" => {
            match threads.to_str().and_then(|t| t.parse().ok()) {
                Some(threads) => bench(
                    Path::new(folder),
                    options,
                    Path::new(queries),
                    Some(threads),
                ),
                None => usage(),
            }
        }
        [command, folder, cases] if command == "golden" => {
            golden(Path::new(folder), options, Path::new(cases))
        }
        [command, folder, text] if command == "grep" => match text.to_str() {
            Some(text) => grep(Path::new(folder), options, text),
            None => usage(),
        },
        _ => usage(),
    }
}

/// Removes `flag` and the file after it. `None` when the file is missing.
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
    Some(Some(file))
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
    println!(
        "index memory: {:.1} MB",
        index.heap_bytes() as f64 / 1_048_576.0
    );
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
                    println!(
                        "saved {:.1} MB in {:.1} ms",
                        size as f64 / 1_048_576.0,
                        ms(started)
                    );
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

fn search(folder: &Path, options: &Options) -> ExitCode {
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let searcher = Searcher::default();
    eprintln!("type a search and press Enter; Ctrl-D quits");
    for line in std::io::stdin().lock().lines() {
        let Ok(query) = line else { break };
        let started = Instant::now();
        let hits = searcher
            .search(&index, &query, searcher.ticket(), 10)
            .unwrap_or_default();
        println!("{query:?} in {:.1} ms", ms(started));
        for hit in hits {
            println!("{:>8}  {}", hit.score, index.path(hit.entry).display());
        }
    }
    ExitCode::SUCCESS
}

fn read(file: &Path) -> Option<String> {
    std::fs::read_to_string(file)
        .inspect_err(|error| eprintln!("tl: cannot read {}: {error}", file.display()))
        .ok()
}

fn bench(folder: &Path, options: &Options, query_file: &Path, threads: Option<usize>) -> ExitCode {
    let Some(text) = read(query_file) else {
        return ExitCode::FAILURE;
    };
    let queries: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').next().unwrap_or(line))
        .filter(|query| !query.trim().is_empty())
        .collect();
    if queries.is_empty() {
        eprintln!("tl: no queries in {}", query_file.display());
        return ExitCode::FAILURE;
    }
    let Some(index) = load(folder, options) else {
        return ExitCode::FAILURE;
    };
    let searcher = threads.map_or_else(Searcher::default, Searcher::with_threads);

    let mut times: Vec<(f64, &str)> = Vec::new();
    for query in &queries {
        for typed in keystrokes(query) {
            let started = Instant::now();
            searcher.search(&index, typed, searcher.ticket(), 50);
            times.push((ms(started), typed));
        }
    }
    times.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at = |share: usize| times[(times.len() * share / 100).min(times.len() - 1)].0;
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
                    let (searcher, index) = (&searcher, &index);
                    scope.spawn(move || searcher.search(index, query, ticket, 50).is_some())
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
    let searcher = Searcher::default();

    let mut in_top_3 = 0;
    for (query, expected) in &cases {
        let hits = searcher
            .search(&index, query, searcher.ticket(), 100)
            .unwrap_or_default();
        let rank = hits
            .iter()
            .position(|hit| index.path(hit.entry) == *expected);
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
                println!("{:>10}  {}", hit.score, index.path(hit.entry).display());
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
    let first = Mutex::new(None);
    let report = grep.search(&index, text, grep.ticket(), 100, |found| {
        first.lock().unwrap().get_or_insert_with(|| ms(started));
        println!("{}:{}: {}", found.path.display(), found.line, found.text);
    });
    let Some(report) = report else {
        return ExitCode::FAILURE;
    };
    let first = first
        .into_inner()
        .unwrap()
        .map_or_else(String::new, |first| {
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
    fn leaves_a_second_copy_of_the_flag_for_usage_to_refuse() {
        let mut line = args("index ~ --load a --load b");
        assert_eq!(take(&mut line, "--load"), Some(Some("a".into())));
        assert_eq!(line, args("index ~ --load b"));
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
}
