use std::ffi::OsString;
use std::io::BufRead;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;
use telescope_core::{Index, Searcher};

const USAGE: &str = "usage:
  tl index <folder>                       scan and report the index size
  tl search <folder>                      scan, then search each line you type
  tl bench <folder> <queries> [threads]   time every keystroke of each query";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("tl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [command, folder] if command == "index" => match load(Path::new(folder)) {
            Some(_) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        },
        [command, folder] if command == "search" => search(Path::new(folder)),
        [command, folder, queries] if command == "bench" => {
            bench(Path::new(folder), Path::new(queries), None)
        }
        [command, folder, queries, threads] if command == "bench" => {
            match threads.to_str().and_then(|t| t.parse().ok()) {
                Some(threads) => bench(Path::new(folder), Path::new(queries), Some(threads)),
                None => usage(),
            }
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::FAILURE
}

fn load(folder: &Path) -> Option<Index> {
    let started = Instant::now();
    let scan = match telescope_core::scan(folder) {
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

fn search(folder: &Path) -> ExitCode {
    let Some(index) = load(folder) else {
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

fn bench(folder: &Path, query_file: &Path, threads: Option<usize>) -> ExitCode {
    let text = match std::fs::read_to_string(query_file) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("tl: cannot read {}: {error}", query_file.display());
            return ExitCode::FAILURE;
        }
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
    let Some(index) = load(folder) else {
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
