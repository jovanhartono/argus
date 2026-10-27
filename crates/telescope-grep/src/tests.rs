use super::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use telescope_core::{Clutter, Ignore};

struct Folder {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn folder(files: &[(&str, &[u8])]) -> Folder {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    for (name, contents) in files {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    Folder { _dir: dir, root }
}

fn index(folder: &Folder) -> Index {
    telescope_core::scan(&folder.root, Clutter::Skip, &Ignore::default())
        .unwrap()
        .index
}

/// Each line found as "path:line:text", sorted, and the report.
fn grep_with(grep: &Grep, folder: &Folder, text: &str, limit: usize) -> (Vec<String>, Report) {
    let index = index(folder);
    let lines = Mutex::new(Vec::new());
    let report = grep
        .search(&index, text, grep.ticket(), limit, |found| {
            assert_eq!(index.path(found.entry), found.path);
            let path = found.path.strip_prefix(&folder.root).unwrap();
            let path = path.to_string_lossy().replace('\\', "/");
            lines
                .lock()
                .unwrap()
                .push(format!("{path}:{}:{}", found.line, found.text));
        })
        .unwrap();
    let mut lines = lines.into_inner().unwrap();
    lines.sort();
    assert_eq!(lines.len(), report.lines_found);
    (lines, report)
}

fn grep(folder: &Folder, text: &str) -> (Vec<String>, Report) {
    grep_with(&Grep::with_threads(2), folder, text, 1000)
}

#[test]
fn finds_each_matching_line_with_its_number() {
    let folder = folder(&[
        (
            "notes.md",
            b"# Plan\nbuy milk\n\n  call the plumber  \nmilk again\n",
        ),
        ("src/main.rs", b"fn main() {\n    let milk = 1;\n}\n"),
        ("empty.txt", b""),
    ]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(
        lines,
        [
            "notes.md:2:buy milk",
            "notes.md:5:milk again",
            "src/main.rs:2:let milk = 1;"
        ]
    );
    assert_eq!(report.files_searched, 3);
    assert_eq!(grep(&folder, "plumber").0, ["notes.md:4:call the plumber"]);
}

#[test]
fn lower_case_finds_any_case_and_upper_case_only_itself() {
    let folder = folder(&[(
        "a.txt",
        "Hello\nhello\nHELLO\nÉcole\nécole\nÉCOLE\n".as_bytes(),
    )]);
    let texts = |text| -> Vec<String> {
        let (lines, _) = grep(&folder, text);
        lines
            .iter()
            .map(|l| l["a.txt:".len()..].to_owned())
            .collect()
    };
    assert_eq!(texts("hello"), ["1:Hello", "2:hello", "3:HELLO"]);
    assert_eq!(texts("Hello"), ["1:Hello"]);
    assert_eq!(texts("HELLO"), ["3:HELLO"]);
    assert_eq!(texts("école"), ["4:École", "5:école", "6:ÉCOLE"]);
    assert_eq!(texts("École"), ["4:École"]);
}

#[test]
fn text_is_matched_letter_for_letter_never_as_a_pattern() {
    let folder = folder(&[("a.txt", b"a.b\naxb\nf(x)\n[x]*\n")]);
    assert_eq!(grep(&folder, "a.b").0, ["a.txt:1:a.b"]);
    assert_eq!(grep(&folder, "f(x").0, ["a.txt:3:f(x)"]);
    assert_eq!(grep(&folder, "]*").0, ["a.txt:4:[x]*"]);
}

#[test]
fn empty_or_two_line_text_finds_nothing() {
    let folder = folder(&[("a.txt", b"one\ntwo\n")]);
    assert_eq!(grep(&folder, ""), (vec![], Report::default()));
    assert_eq!(grep(&folder, "one\ntwo"), (vec![], Report::default()));
}

#[test]
fn trims_line_endings_and_reads_utf16() {
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(
        "first\r\nsecond milk\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let folder = folder(&[("dos.txt", b"\tmilk\r\nno\r\n"), ("utf16.txt", &utf16)]);
    assert_eq!(
        grep(&folder, "milk").0,
        ["dos.txt:1:milk", "utf16.txt:2:second milk"]
    );
}

#[test]
fn skips_binary_files() {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR milk".to_vec();
    png.extend([0; 100]);
    let folder = folder(&[("image.png", &png), ("a.txt", b"milk\n")]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(lines, ["a.txt:1:milk"]);
    assert_eq!(report.binary, 1);
    assert_eq!(report.files_searched, 1);
}

#[test]
fn skips_files_over_the_size_cap() {
    let limit = usize::try_from(MAX_FILE_BYTES).unwrap();
    let mut at_cap = b"milk\n".repeat(limit / 5);
    at_cap.resize(limit, b'\n');
    let mut over_cap = at_cap.clone();
    over_cap.push(b'\n');
    let folder = folder(&[("at.txt", &at_cap), ("over.txt", &over_cap)]);
    let (lines, report) = grep(&folder, "milk");
    assert!(lines.iter().all(|line| line.starts_with("at.txt:")));
    assert_eq!(report.too_large, 1);
    assert_eq!(report.files_searched, 1);
}

#[test]
fn a_line_too_long_to_hold_ends_only_its_own_file() {
    let mut minified = b"milk ".to_vec();
    minified.resize(MAX_LINE_BYTES + 10, b'x');
    let folder = folder(&[("bundle.min.js", &minified), ("a.txt", b"milk\n")]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(lines, ["a.txt:1:milk"]);
    assert_eq!(report.too_large, 1);
}

#[test]
fn follows_the_scan_rules_and_never_links() {
    let folder = folder(&[
        ("node_modules/lib/index.js", b"milk\n"),
        (".git/config", b"milk\n"),
        ("Notes.app/Contents/Info.plist", b"milk\n"),
        ("src/a.js", b"milk\n"),
    ]);
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        folder.root.join("node_modules/lib/index.js"),
        folder.root.join("src/link.js"),
    )
    .unwrap();
    assert_eq!(grep(&folder, "milk").0, ["src/a.js:1:milk"]);
}

#[test]
fn never_opens_a_cloud_only_file() {
    let folder = folder(&[("cloud.txt", b"milk\n"), ("local.txt", b"milk\n")]);
    // A file that cannot be opened shows whether the check came first.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = fs::Permissions::from_mode(0o000);
        fs::set_permissions(folder.root.join("cloud.txt"), locked).unwrap();
    }
    let mut grep = Grep::with_threads(2);
    grep.cloud_only = |path, _| path.ends_with("cloud.txt");
    let (lines, report) = grep_with(&grep, &folder, "milk", 10);
    assert_eq!(lines, ["local.txt:1:milk"]);
    assert_eq!(report.cloud_only, 1);
    assert_eq!(report.unreadable, 0);
}

#[cfg(unix)]
#[test]
fn counts_files_it_may_not_open() {
    use std::os::unix::fs::PermissionsExt;
    let folder = folder(&[("secret.txt", b"milk\n"), ("a.txt", b"milk\n")]);
    let locked = fs::Permissions::from_mode(0o000);
    fs::set_permissions(folder.root.join("secret.txt"), locked).unwrap();
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(lines, ["a.txt:1:milk"]);
    assert_eq!(report.unreadable, 1);
}

#[test]
fn stops_at_the_limit() {
    let files: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("{i}.txt"), b"milk\nmilk\nmilk\n".to_vec()))
        .collect();
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_slice()))
        .collect();
    let folder = folder(&files);
    let grep = Grep::with_threads(4);
    for limit in [0, 1, 7, 50, 120, 500] {
        let (lines, _) = grep_with(&grep, &folder, "milk", limit);
        assert_eq!(lines.len(), limit.min(120));
    }
}

#[test]
fn a_newer_ticket_cancels_an_older_search() {
    let folder = folder(&[("a.txt", b"milk\n")]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let old = grep.ticket();
    let _new = grep.ticket();
    assert_eq!(grep.search(&index, "milk", old, 10, |_| panic!()), None);
}

#[test]
fn a_replaced_search_stops_inside_a_big_file() {
    // Every line matches, so the search is replaced at its first line, right at the start.
    let limit = usize::try_from(MAX_FILE_BYTES).unwrap();
    let mut big = b"milk and more words to make a longer line\n".repeat(limit / 43);
    big.truncate(limit);
    let folder = folder(&[("big.txt", &big)]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let replaced = Mutex::new(None);
    let lines = AtomicUsize::new(0);
    let result = grep.search(&index, "milk", grep.ticket(), usize::MAX, |_| {
        lines.fetch_add(1, Ordering::Relaxed);
        let mut replaced = replaced.lock().unwrap();
        if replaced.is_none() {
            grep.ticket();
            *replaced = Some(Instant::now());
        }
    });
    let latency = replaced.into_inner().unwrap().unwrap().elapsed();
    assert_eq!(result, None);
    assert_eq!(lines.load(Ordering::Relaxed), 1);
    assert!(latency < Duration::from_millis(50), "{latency:?}");
}

#[test]
fn a_replaced_search_stops_between_files() {
    let files: Vec<(String, Vec<u8>)> = (0..2000)
        .map(|i| (format!("d{}/{i}.txt", i % 20), b"milk\n".to_vec()))
        .collect();
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_slice()))
        .collect();
    let folder = folder(&files);
    let index = index(&folder);
    let grep = Grep::with_threads(4);
    let lines = AtomicUsize::new(0);
    let result = grep.search(&index, "milk", grep.ticket(), usize::MAX, |_| {
        if lines.fetch_add(1, Ordering::Relaxed) == 0 {
            grep.ticket();
        }
    });
    assert_eq!(result, None);
    // Other threads may finish a few files before they see the new ticket.
    assert!(lines.load(Ordering::Relaxed) < 100, "{lines:?}");
}

#[test]
fn searches_in_a_row_start_fresh() {
    let folder = folder(&[("a.txt", b"milk\n")]);
    let grep = Grep::with_threads(2);
    assert_eq!(grep_with(&grep, &folder, "milk", 0).0.len(), 0);
    assert_eq!(grep_with(&grep, &folder, "milk", 10).0.len(), 1);
}

fn excerpt_of(line: &[u8], text: &str) -> (String, Range<usize>) {
    let (cut, matched) = excerpt(&Literal::new(text).unwrap(), line);
    (cut.into_owned(), matched)
}

#[test]
fn a_long_line_is_cut_around_the_match() {
    let mut line = "x".repeat(1000);
    line.replace_range(500..504, "milk");
    let (text, matched) = excerpt_of(line.as_bytes(), "milk");
    assert_eq!(text.len(), MAX_TEXT_BYTES);
    assert_eq!(matched, BEFORE_MATCH..BEFORE_MATCH + 4);

    let (text, matched) = excerpt_of(format!("{}milk", "x".repeat(1000)).as_bytes(), "milk");
    assert_eq!(&text[matched], "milk");
    assert!(text.ends_with("milk"));
}

#[test]
fn a_line_with_broken_letters_still_points_at_the_match() {
    let (text, matched) = excerpt_of(b"\xff\xfe caf\xc3\xa9 milk", "milk");
    assert_eq!(text, "\u{fffd}\u{fffd} café milk");
    assert_eq!(&text[matched], "milk");
}

/// A fixed-seed xorshift, so a failing case comes back on every run.
fn random_below() -> impl FnMut(usize) -> usize {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    move |below| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        usize::try_from(seed % below as u64).unwrap()
    }
}

/// Lines of random letters, one to four bytes long, with the match anywhere: the cut is
/// never longer than allowed, never splits a letter, and always shows the match.
#[test]
fn any_cut_keeps_whole_letters_and_the_match() {
    let letters = ["a", " ", "é", "€", "𝄞", "\u{fffd}"];
    let mut random = random_below();
    for _ in 0..2000 {
        let mut line = String::new();
        for _ in 0..random(600) {
            line.push_str(letters[random(letters.len())]);
        }
        let mut at = random(line.len() + 1);
        while !line.is_char_boundary(at) {
            at -= 1;
        }
        line.insert_str(at, "Milk");
        let (text, matched) = excerpt_of(line.as_bytes(), "Milk");
        assert!(text.len() <= MAX_TEXT_BYTES, "{line}");
        assert_eq!(&text[matched], "Milk", "{line}");
        assert!(line.trim().contains(&text), "{line}");
    }
}

/// Files are not always valid UTF-8: long runs of stray bytes never break the cut.
#[test]
fn any_cut_of_broken_bytes_still_shows_the_match() {
    let pieces: [&[u8]; 5] = [b"a", b"\x80", b"\xff", "é".as_bytes(), b"\xe2\x82"];
    let mut random = random_below();
    for _ in 0..2000 {
        let mut line = Vec::new();
        for _ in 0..random(800) {
            line.extend_from_slice(pieces[random(pieces.len())]);
        }
        let at = random(line.len() + 1);
        line.splice(at..at, *b"Milk");
        let (text, matched) = excerpt_of(&line, "Milk");
        // Each broken byte becomes a three-byte stand-in letter.
        assert!(text.len() <= 3 * MAX_TEXT_BYTES, "{line:?}");
        assert_eq!(&text[matched], "Milk", "{line:?}");
    }
}
