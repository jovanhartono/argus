use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
fn version_prints_the_package_version() {
    let output = tl(&["--version"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("tl {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Both are bumped by hand for a release, and a mismatch would ship an app and a `tl` that
/// disagree about which release they are.
#[test]
fn the_mac_app_has_the_same_version_as_the_crates() {
    let plist = include_str!("../../../apps/macos/Info.plist");
    let key = "<key>CFBundleShortVersionString</key>";
    let after = &plist[plist.find(key).expect("no CFBundleShortVersionString") + key.len()..];
    let value = after.trim_start().strip_prefix("<string>").unwrap();
    let value = &value[..value.find("</string>").unwrap()];
    assert_eq!(value, env!("CARGO_PKG_VERSION"));
}

/// `tl search <dir> --in <within> --all`, run from `dir`, typing `a.txt`.
fn search_in(dir: &Path, within: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tl"))
        .args(["search", &path(dir, ""), "--in", within, "--all"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a.txt\n").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn search_in_a_folder_finds_only_what_is_inside_it() {
    let dir = folder();
    fs::create_dir(dir.path().join("other")).unwrap();
    fs::write(dir.path().join("other/a.txt"), "").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("files"), dir.path().join("link")).unwrap();
    let inside = format!("{}\n", Path::new("files").join("a.txt").display());
    let mut ways = vec![path(dir.path(), "files"), "files".to_owned()];
    if cfg!(unix) {
        ways.push("link".to_owned());
    }
    for within in ways {
        let output = search_in(dir.path(), &within);
        assert!(output.status.success(), "{within}: {}", stderr(&output));
        let out = stdout(&output);
        assert!(out.contains(&inside), "{within}: {out}");
        assert!(!out.contains("other"), "{within}: {out}");
    }

    for within in [path(dir.path(), "nowhere"), path(dir.path(), "files/a.txt")] {
        let output = search_in(dir.path(), &within);
        assert_eq!(output.status.code(), Some(1), "{within}");
        assert!(stderr(&output).contains("is not a folder in the index"));
    }

    let queries = path(dir.path(), "queries.txt");
    let within = path(dir.path(), "files");
    let root = path(dir.path(), "");
    let output = tl(&["bench", &root, &queries, "1", "--in", &within, "--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("inside "), "{}", stdout(&output));

    let output = tl(&["grep", &root, "milk", "--in", &within, "--all"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("usage:"));
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

#[test]
fn a_file_opened_often_climbs_the_golden_list_only_with_frecency() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("files");
    fs::create_dir_all(files.join("a/b")).unwrap();
    fs::write(files.join("plan.md"), "").unwrap();
    fs::write(files.join("a/b/plan.md"), "").unwrap();
    let deep = path(&files, "a/b/plan.md");
    let golden = path(dir.path(), "golden.tsv");
    fs::write(&golden, format!("plan\t{deep}\n")).unwrap();
    let (files, list) = (
        path(dir.path(), "files"),
        path(dir.path(), "opens/frecency"),
    );

    let output = tl(&["open-record", &deep, "--frecency", &list]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(": score 1.00, one of 1 files opened in "));
    let output = tl(&["open-record", &deep, "--frecency", &list]);
    assert!(stdout(&output).contains(": score 2.00, one of 1 files opened in "));

    let without = stdout(&tl(&["golden", &files, &golden, "--all"]));
    assert!(without.contains("   2  plan\n"), "{without}");
    assert!(without.contains("in the top 3: 1 of 1"), "{without}");
    let with = stdout(&tl(&[
        "golden",
        &files,
        &golden,
        "--all",
        "--frecency",
        &list,
    ]));
    assert!(
        with.contains("frecency: 1 of 1 files opened are in the index"),
        "{with}"
    );
    assert!(with.contains("   1  plan\n"), "{with}");
}

#[test]
fn open_record_refuses_a_path_that_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    let list = path(dir.path(), "frecency");
    let output = tl(&[
        "open-record",
        &path(dir.path(), "missing"),
        "--frecency",
        &list,
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cannot find"),
        "{}",
        stderr(&output)
    );
    assert!(!dir.path().join("frecency").exists());
}

#[test]
fn a_damaged_list_starts_again() {
    let dir = folder();
    let (files, list) = (path(dir.path(), "files"), path(dir.path(), "frecency"));
    assert!(
        tl(&["open-record", &files, "--frecency", &list])
            .status
            .success()
    );
    let bytes = fs::read(&list).unwrap();
    fs::write(&list, &bytes[..bytes.len() - 1]).unwrap();
    let output = tl(&["open-record", &files, "--frecency", &list]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("starting a new list"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).contains(": score 1.00, one of 1 files opened"));
    let output = tl(&["open-record", &files, "--frecency", &list]);
    assert_eq!(stderr(&output), "");
    assert!(stdout(&output).contains(": score 2.00, one of 1 files opened"));
}

#[test]
fn a_file_that_is_not_a_list_is_left_as_it_is() {
    let dir = folder();
    let list = path(dir.path(), "frecency");
    fs::write(&list, "garbage").unwrap();
    let output = tl(&[
        "open-record",
        &path(dir.path(), "files"),
        "--frecency",
        &list,
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("left as it is"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read(&list).unwrap(), b"garbage");
}

#[test]
fn open_record_needs_a_list() {
    let dir = folder();
    let output = tl(&["open-record", &path(dir.path(), "files")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("usage:"));
}

#[test]
fn a_missing_list_ranks_nothing_and_says_nothing() {
    let dir = folder();
    let list = path(dir.path(), "missing");
    let queries = path(dir.path(), "queries.txt");
    let files = path(dir.path(), "files");
    let output = tl(&["bench", &files, &queries, "1", "--all", "--frecency", &list]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("cannot open"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("frecency: 0 of 0 files opened are in the index"),
        "{}",
        stdout(&output)
    );
    assert!(!Path::new(&list).exists());
}

/// A link counts as the link the scan lists, not the file it points to, and a relative path
/// as the real one; the search then ranks the opened link above a file of the same name
/// higher up.
#[cfg(unix)]
#[test]
fn open_record_counts_a_link_itself_and_resolves_its_folder() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = folder();
    let files = dir.path().join("files");
    fs::write(files.join("link.txt"), "").unwrap();
    fs::create_dir(files.join("deep")).unwrap();
    std::os::unix::fs::symlink(files.join("a.txt"), files.join("deep/link.txt")).unwrap();
    let list = path(dir.path(), "frecency");
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .args(["open-record", "deep/../deep/link.txt", "--frecency", &list])
        .current_dir(&files)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let link = fs::canonicalize(&files).unwrap().join("deep/link.txt");
    assert!(
        stdout(&output).starts_with(&format!("{}: score 1.00", link.display())),
        "{}",
        stdout(&output)
    );

    let search = |more: &[&str]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tl"))
            .args(["search", files.to_str().unwrap(), "--all"])
            .args(more)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"link\n").unwrap();
        stdout(&child.wait_with_output().unwrap())
    };
    let first = |out: &str| {
        let mut lines = out.lines().skip_while(|line| !line.starts_with("\"link\""));
        lines.nth(1).unwrap().ends_with(&link.display().to_string())
    };
    let out = search(&[]);
    assert!(!first(&out), "{out}");
    let out = search(&["--frecency", &list]);
    assert!(
        out.contains("frecency: 1 of 1 files opened are in the index"),
        "{out}"
    );
    assert!(first(&out), "{out}");
}

#[test]
fn a_volume_joins_the_index_and_survives_saving() {
    let dir = folder();
    let drive = tempfile::tempdir().unwrap();
    fs::create_dir(drive.path().join("photos")).unwrap();
    fs::write(drive.path().join("photos/beach.jpg"), "").unwrap();
    let (files, saved) = (path(dir.path(), "files"), path(dir.path(), "files.idx"));
    let volume = drive.path().to_str().unwrap();
    let output = tl(&[
        "index", &files, "--all", "--volume", volume, "--save", &saved,
    ]);
    let out = stdout(&output);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(out.contains(": 3 entries, +"), "{out}");
    // The folder and a.txt, then the drive, photos and the beach.
    assert!(out.contains("5 entries (3 folders)"), "{out}");
    // Opened again, its paths are whole, built up from the drive's own entry.
    let output = tl_with_input(&["search", &files, "--all", "--load", &saved], "beach\n");
    let out = stdout(&output);
    assert!(out.contains("5 entries (3 folders)"), "{out}");
    let beach = fs::canonicalize(drive.path())
        .unwrap()
        .join("photos/beach.jpg");
    assert!(out.contains(&format!("  {}\n", beach.display())), "{out}");

    // Opened with the drive already in it, as when trying more changes on a saved index.
    let output = tl(&[
        "index", &files, "--all", "--load", &saved, "--volume", volume,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("already in the index"));
    assert!(stdout(&output).contains("5 entries (3 folders)"));

    let output = tl(&["index", &files, "--all", "--volume", &files]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cannot add") && stderr(&output).contains("exists"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn changes_on_a_volume_are_read_again_and_not_skipped() {
    let dir = folder();
    let drive = tempfile::tempdir().unwrap();
    let real = fs::canonicalize(drive.path()).unwrap();
    fs::create_dir(real.join("photos")).unwrap();
    let changes = path(dir.path(), "changes.txt");
    fs::write(&changes, format!("{}\n", real.join("photos").display())).unwrap();
    let output = tl(&[
        "index",
        &path(dir.path(), "files"),
        "--all",
        "--volume",
        real.to_str().unwrap(),
        "--changes",
        &changes,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("read 1 changed folders again"), "{out}");
    assert!(!stderr(&output).contains("skipped"), "{}", stderr(&output));
}

#[cfg(unix)]
#[test]
fn folders_on_a_volume_not_fully_read_are_counted() {
    use std::os::unix::fs::PermissionsExt;
    let dir = folder();
    let drive = tempfile::tempdir().unwrap();
    let locked = drive.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let files = path(dir.path(), "files");
    let output = tl(&[
        "index",
        &files,
        "--all",
        "--volume",
        drive.path().to_str().unwrap(),
    ]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("folders not fully read: 1"));
}

fn tl_with_input(args: &[&str], input: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tl"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
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
