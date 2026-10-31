//! How long ⌘T takes to its first line around a burst of 3,000 new files, as an npm install
//! leaves them: Argus's own capped Spotlight ask, then the files looked at and read on the text
//! search threads, as the app does it. Off by default; it writes ~/ArgusBurst-<pid> and deletes it
//! after: `cargo test -p argus-grep --release --test burst -- --ignored --nocapture`.

use argus_grep::{Grep, spotlight};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Milliseconds to the first line and to the last, and the lines found.
fn command_t(grep: &Grep, notes: &Path) -> (Option<f64>, f64, usize) {
    let started = Instant::now();
    let named = spotlight::files("budget", &[notes.to_path_buf()], &|| false).unwrap_or_default();
    let mut files: Vec<_> = grep.install(|| {
        let file = |file: PathBuf| {
            let metadata = fs::symlink_metadata(&file).ok().filter(|m| m.is_file())?;
            Some((metadata.modified().ok(), file))
        };
        named.into_iter().filter_map(file).collect()
    });
    files.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let files: Vec<PathBuf> = files.into_iter().map(|(_, file)| file).collect();
    let first = Cell::new(None);
    let report = grep.search_files(&files, "budget", grep.ticket(), 100, &|| false, |_| {
        first.set(first.get().or(Some(ms(started))));
    });
    let lines = report.map_or(0, |report| report.lines_found);
    (first.get(), ms(started), lines)
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn watch(what: &str, grep: &Grep, notes: &Path, for_how_long: Duration) {
    let started = Instant::now();
    let (mut firsts, mut lasts, mut empty) = (Vec::new(), Vec::new(), 0);
    while started.elapsed() < for_how_long {
        let (first, last, lines) = command_t(grep, notes);
        // A search Spotlight answered with nothing ends fast and would pass for a quick one.
        match first {
            Some(first) => {
                firsts.push(first);
                lasts.push(last);
            }
            None => empty += 1,
        }
        println!(
            "BURST {what} +{:.1}s: first line {:.0} ms, all {last:.0} ms, {lines} lines",
            started.elapsed().as_secs_f64(),
            first.unwrap_or(f64::NAN),
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(!firsts.is_empty(), "{what}: no search found a line");
    let at = |times: &mut Vec<f64>, share: usize| {
        times.sort_by(f64::total_cmp);
        times[(times.len() * share / 100).min(times.len() - 1)]
    };
    println!(
        "BURST {what}: {} searches ({empty} found nothing, left out), first line p50 {:.0} p95 {:.0} ms, all p95 {:.0} ms",
        firsts.len(),
        at(&mut firsts, 50),
        at(&mut firsts, 95),
        at(&mut lasts, 95),
    );
}

struct Gone(PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore]
fn command_t_around_a_burst() {
    // A folder of this run's own: two worktrees running it at once must not delete each other's.
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let root = home.join(format!("ArgusBurst-{}", std::process::id()));
    fs::create_dir(&root).expect("a new folder, never one already there");
    let _gone = Gone(root.clone());
    let notes = root.join("Notes");
    fs::create_dir(&notes).unwrap();
    for n in 0..150 {
        let text = format!("Note {n}\nBudget for the offsite needs sign-off from finance.\n");
        fs::write(notes.join(format!("note-{n}.md")), text).unwrap();
    }
    let grep = Grep::default();
    let started = Instant::now();
    while command_t(&grep, &notes).2 < 100 && started.elapsed() < Duration::from_secs(90) {
        std::thread::sleep(Duration::from_secs(1));
    }
    let took = started.elapsed().as_secs_f64();
    assert!(
        command_t(&grep, &notes).2 >= 100,
        "Spotlight never listed the notes in 90 s"
    );
    println!("BURST Spotlight took in the notes after {took:.1}s");
    watch("quiet", &grep, &notes, Duration::from_secs(5));
    let modules = root.join("node_modules");
    for n in 0..3000 {
        let folder = modules.join(format!("pkg-{}/lib", n / 10));
        fs::create_dir_all(&folder).unwrap();
        let text = format!("module.exports = {n};\n");
        fs::write(folder.join(format!("file{}.js", n % 10)), text).unwrap();
    }
    watch("after the install", &grep, &notes, Duration::from_secs(20));
    fs::remove_dir_all(&modules).unwrap();
    watch("after deleting it", &grep, &notes, Duration::from_secs(12));
}
