use std::ffi::OsString;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;
use telescope_core::{Clutter, Index, Searcher};

const USAGE: &str = "usage:
  tl index <folder>                       scan and report the index size
  tl search <folder>                      scan, then search each line you type
  tl bench <folder> <queries> [threads]   time every keystroke of each query
  tl golden <folder> <golden>             rank the expected file of each search
add --all to include hidden folders, ~/Library, the insides of packages like Photos Library,
and tool downloads and caches like node_modules";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let clutter = if args.iter().any(|arg| arg == "--all") {
        Clutter::Keep
    } else {
        Clutter::Skip
    };
    args.retain(|arg| arg != "--all");
    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("tl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [command, folder] if command == "index" => match load(Path::new(folder), clutter) {
            Some(_) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        },
        [command, folder] if command == "search" => search(Path::new(folder), clutter),
        [command, folder, queries] if command == "bench" => {
            bench(Path::new(folder), clutter, Path::new(queries), None)
        }
        [command, folder, queries, threads] if command == "bench" => {
            match threads.to_str().and_then(|t| t.parse().ok()) {
                Some(threads) => bench(
                    Path::new(folder),
                    clutter,
                    Path::new(queries),
                    Some(threads),
                ),
                None => usage(),
            }
        }
        [command, folder, cases] if command == "golden" => {
            golden(Path::new(folder), clutter, Path::new(cases))
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::FAILURE
}

fn load(folder: &Path, clutter: Clutter) -> Option<Index> {
    let started = Instant::now();
    let scan = match telescope_core::scan(folder, clutter) {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!("tl: cannot scan {}: {error}", folder.display());
            return None;
        }
    };
    let seconds = started.elapsed().as_secs_f64();

    let index = scan.index;
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
    println!("scan time: {seconds:.2} s");
    if scan.unreadable_folders > 0 {
        println!("folders not fully read: {}", scan.unreadable_folders);
    }
    Some(index)
}

fn search(folder: &Path, clutter: Clutter) -> ExitCode {
    let Some(index) = load(folder, clutter) else {
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

fn bench(folder: &Path, clutter: Clutter, query_file: &Path, threads: Option<usize>) -> ExitCode {
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
    let Some(index) = load(folder, clutter) else {
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
fn golden(folder: &Path, clutter: Clutter, case_file: &Path) -> ExitCode {
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
    let Some(index) = load(folder, clutter) else {
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
