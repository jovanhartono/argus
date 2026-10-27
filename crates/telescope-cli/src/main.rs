use std::ffi::OsString;
use std::io::BufRead;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use telescope_core::{Access, Clutter, Ignore, Index, Opened, Searcher, Settings};
use telescope_grep::Grep;

const USAGE: &str = "usage:
  tl index <folder>                            scan and report the index size
  tl search <folder>                           scan, then search each line you type
  tl bench <folder> <queries> [threads]        time every keystroke of each query
  tl golden <folder> <golden>                  rank the expected file of each search
  tl grep <folder> <text>                      scan, then show lines holding the text
  tl grep-bench <folder> <queries> [threads]   time the first and the last line of each text
add --all to include hidden folders, ~/Library, the insides of packages like Photos Library,
and tool downloads and caches like node_modules
add --load <file> to open a saved index instead of scanning (it scans when the file is missing,
damaged or built with other settings, and does not catch up on changes made since it was saved),
and --save <file> to save the index afterwards

your own ignore list, ~/.config/telescope/ignore, takes one folder per line
(--all leaves it out too):
  Old Projects        skip every folder with this name
  ~/Movies/Exports    skip this one folder
  !~/.config/nvim     bring back a folder Telescope skips, even inside a skipped one
  # a comment";

/// Lines `tl grep` shows, as many as the window would.
const GREP_LIMIT: usize = 100;

#[derive(Debug, PartialEq)]
struct Options {
    clutter: Clutter,
    load: Option<PathBuf>,
    save: Option<PathBuf>,
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
}

fn main() -> ExitCode {
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
    }
}

/// `None` when the line does not fit any command, so usage is shown.
fn parse(mut args: Vec<OsString>) -> Option<(Command, Options)> {
    let clutter = if args.iter().any(|arg| arg == "--all") {
        Clutter::Keep
    } else {
        Clutter::Skip
    };
    args.retain(|arg| arg != "--all");
    let load = take(&mut args, "--load")?;
    let save = take(&mut args, "--save")?;
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
            text: text.to_str()?.to_owned(),
        },
        [command, folder, queries, rest @ ..] if command == "grep-bench" => Command::GrepBench {
            folder: folder.into(),
            queries: queries.into(),
            threads: threads(rest)?,
        },
        _ => return None,
    };
    Some((
        command,
        Options {
            clutter,
            load,
            save,
        },
    ))
}

/// The optional thread count after a bench's query file. Zero would mean every core to rayon,
/// so it is refused rather than read as that.
fn threads(rest: &[OsString]) -> Option<Option<usize>> {
    match rest {
        [] => Some(None),
        [threads] => Some(Some(
            threads.to_str()?.parse::<NonZero<usize>>().ok()?.get(),
        )),
        _ => None,
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
    let scan = match scan {
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

    let index = &scan.index;
    println!(
        "{} entries ({} folders), {} distinct names",
        index.len(),
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
        Some(file) if opened_at.is_some() && options.load.as_ref() == Some(file) => {
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

/// One query per line, up to a tab; lines starting with `#` are comments.
fn queries(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').next().unwrap_or(line))
        .filter(|query| !query.trim().is_empty())
        .collect()
}

/// The time below which `share` percent of the sorted `times` fall, as `bench` counts it.
fn percentile(times: &[f64], share: usize) -> f64 {
    times[(times.len() * share / 100).min(times.len() - 1)]
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
    let report = grep.search(&index, text, grep.ticket(), GREP_LIMIT, |found| {
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
    let indexed_peak = peak_memory();
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
    let total_times: Vec<f64> = totals.iter().map(|(time, _)| *time).collect();
    println!(
        "all lines:  p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        percentile(&total_times, 50),
        percentile(&total_times, 95),
        percentile(&total_times, 100)
    );
    for (time, query) in totals.iter().rev().take(3) {
        println!("  slowest: {query:?} {time:.1} ms");
    }
    if let (Some(indexed), Some(peak)) = (indexed_peak, peak_memory()) {
        println!(
            "peak memory: {:.1} MB with the index loaded, {:.1} MB after the searches",
            mb(indexed),
            mb(peak)
        );
    }
    ExitCode::SUCCESS
}

/// The most memory this process has held, as `footprint` and Activity Monitor count it.
#[cfg(target_os = "macos")]
fn peak_memory() -> Option<u64> {
    let pid = i32::try_from(std::process::id()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
    // SAFETY: RUSAGE_INFO_V4 makes the call write one whole rusage_info_v4, which `info` has
    // room for, and `info` is read only when the call reports success.
    unsafe {
        (libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) == 0)
            .then(|| info.assume_init().ri_lifetime_max_phys_footprint)
    }
}

#[cfg(not(target_os = "macos"))]
fn peak_memory() -> Option<u64> {
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
    fn leaves_a_second_copy_of_the_flag_for_usage_to_refuse() {
        let mut line = args("index ~ --load a --load b");
        assert_eq!(take(&mut line, "--load"), Some(Some("a".into())));
        assert_eq!(line, args("index ~ --load b"));
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
        assert_eq!(
            command("bench ~ q.txt 4"),
            Some(Command::Bench {
                folder: "~".into(),
                queries: "q.txt".into(),
                threads: Some(4),
            })
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
        assert_eq!(
            command("grep ~ --version"),
            Some(Command::Grep {
                folder: "~".into(),
                text: "--version".into(),
            })
        );
    }

    #[test]
    fn refuses_lines_that_fit_no_command() {
        for line in [
            "",
            "--version extra",
            "index",
            "index ~ extra",
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
        ] {
            assert_eq!(command(line), None, "{line:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_text_that_is_not_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let line = vec!["grep".into(), "~".into(), OsString::from_vec(vec![0xFF])];
        assert!(parse(line).is_none());
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
                save: None,
            }
        );
        let (_, options) = parse(args("grep ~ todo --save b.idx")).unwrap();
        assert_eq!(
            options,
            Options {
                clutter: Clutter::Skip,
                load: None,
                save: Some("b.idx".into()),
            }
        );
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
    }

    #[test]
    fn takes_percentiles_from_sorted_times() {
        assert_eq!(percentile(&[7.0], 50), 7.0);
        assert_eq!(percentile(&[7.0], 100), 7.0);
        let times: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&times, 0), 1.0);
        assert_eq!(percentile(&times, 50), 11.0);
        assert_eq!(percentile(&times, 95), 20.0);
        assert_eq!(percentile(&times, 100), 20.0);
        let times: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&times, 95), 96.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn peak_memory_counts_memory_in_use() {
        let before = peak_memory().unwrap();
        assert!(before > 0);
        let held = std::hint::black_box(vec![1u8; 64 << 20]);
        let after = peak_memory().unwrap();
        assert!(after >= before.max(held.len() as u64), "{before} {after}");
        drop(held);
        assert!(peak_memory().unwrap() >= after);
    }
}
