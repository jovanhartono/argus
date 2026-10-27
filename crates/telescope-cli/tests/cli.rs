use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tl"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// A folder with one line to find, and a query file with one text that finds it and one that
/// does not. `--all` keeps the tests from reading the ignore list of whoever runs them.
fn folder() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("files")).unwrap();
    fs::write(dir.path().join("files/a.txt"), "milk\nbread\n").unwrap();
    fs::write(dir.path().join("queries.txt"), "# texts\nmilk\nnowhere\n").unwrap();
    dir
}

fn path(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_owned()
}

#[test]
fn a_line_that_fits_no_command_shows_usage_and_fails() {
    let output = tl(&["grep", "~"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("usage:"));
    assert_eq!(stdout(&output), "");
}

#[test]
fn grep_shows_each_line_found() {
    let dir = folder();
    let output = tl(&["grep", &path(dir.path(), "files"), "milk", "--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("a.txt:1: milk\n"), "{out}");
    assert!(out.contains("1 lines in 1 files searched"), "{out}");
}

#[test]
fn grep_bench_times_each_text() {
    let dir = folder();
    let output = tl(&[
        "grep-bench",
        &path(dir.path(), "files"),
        &path(dir.path(), "queries.txt"),
        "1",
        "--all",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("\"milk\": 1 lines in 1 files, first "),
        "{out}"
    );
    assert!(
        out.contains("\"nowhere\": 0 lines in 1 files, first none, all "),
        "{out}"
    );
    assert!(out.contains("2 searches on 1 threads"), "{out}");
    assert!(out.contains("(1 of 2 searches found lines)"), "{out}");
    let slowest: Vec<f64> = out
        .lines()
        .filter_map(|line| line.strip_prefix("  slowest: "))
        .map(|line| {
            let ms = line.rsplit(' ').nth(1).unwrap();
            ms.parse().unwrap()
        })
        .collect();
    assert_eq!(slowest.len(), 2, "{out}");
    assert!(slowest[0] >= slowest[1], "{out}");
}

#[test]
fn grep_bench_with_no_line_found_says_so() {
    let dir = folder();
    fs::write(dir.path().join("queries.txt"), "nowhere\n").unwrap();
    let output = tl(&[
        "grep-bench",
        &path(dir.path(), "files"),
        &path(dir.path(), "queries.txt"),
        "--all",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("first line: no search found any"));
}

#[test]
fn grep_bench_fails_without_queries() {
    let dir = folder();
    let files = path(dir.path(), "files");
    let output = tl(&["grep-bench", &files, &path(dir.path(), "missing.txt")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cannot read"),
        "{}",
        stderr(&output)
    );

    fs::write(dir.path().join("queries.txt"), "# only a comment\n\n").unwrap();
    let output = tl(&["grep-bench", &files, &path(dir.path(), "queries.txt")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no queries"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_index_opened_from_the_file_it_is_saved_to_is_not_saved_again() {
    let dir = folder();
    let (files, saved) = (path(dir.path(), "files"), path(dir.path(), "files.idx"));
    let output = tl(&["index", &files, "--all", "--save", &saved]);
    let saved_line = |out: &str| out.lines().any(|line| line.starts_with("saved "));
    assert!(saved_line(&stdout(&output)), "{}", stdout(&output));
    let output = tl(&["index", &files, "--all", "--load", &saved, "--save", &saved]);
    let out = stdout(&output);
    assert!(out.contains("not saved again"), "{out}");
    assert!(!saved_line(&out), "{out}");
}

/// Rust ignores SIGPIPE, so without restoring it every print to a closed pipe panics.
#[cfg(unix)]
#[test]
fn stops_quietly_when_the_reader_goes_away() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    let dir = folder();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tl"))
        .args(["grep", &path(dir.path(), "files"), "milk", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    let errors = stderr(&output);
    assert!(!errors.contains("panicked"), "{errors}");
    // Killed by the closed pipe, or done before the pipe closed.
    let killed = output.status.signal() == Some(libc::SIGPIPE);
    assert!(killed || output.status.success(), "{:?}", output.status);
}
