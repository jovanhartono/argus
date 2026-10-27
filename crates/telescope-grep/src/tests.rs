use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;
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

/// Each line found as "path:line:text", sorted, and the report. Every line must show its
/// match, less any spaces the trimmed line dropped.
fn grep_with(grep: &Grep, folder: &Folder, text: &str, limit: usize) -> (Vec<String>, Report) {
    let index = index(folder);
    let shown = Literal::new(text.trim_ascii());
    let lines = Mutex::new(Vec::new());
    let report = grep
        .search(&index, text, grep.ticket(), limit, |found| {
            assert_eq!(index.path(found.entry), found.path);
            if let Some(shown) = &shown {
                let matched = found.text[found.matched.clone()].trim_ascii();
                let whole = shown.find(matched.as_bytes()).unwrap();
                assert_eq!(whole.map(|m| m.start()..m.end()), Some(0..matched.len()));
            }
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

/// Keeps anyone else from opening `path`, or `None` when this user opens any file anyway,
/// like root.
#[cfg(unix)]
fn lock(path: &Path) -> Option<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    File::open(path).is_err().then_some(())
}

#[cfg(windows)]
fn lock(path: &Path) -> Option<File> {
    use std::os::windows::fs::OpenOptionsExt;
    Some(
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap(),
    )
}

fn many(count: usize, name: impl Fn(usize) -> String, text: &[u8]) -> Folder {
    let files: Vec<(String, &[u8])> = (0..count).map(|i| (name(i), text)).collect();
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, t)| (n.as_str(), *t)).collect();
    folder(&files)
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
        "Hello\nhello\nHELLO\nÉcole\nécole\nÉCOLE\nSTRAẞE\nΟΔΌΣ\n".as_bytes(),
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
    assert_eq!(texts("ße"), ["7:STRAẞE"]);
    assert_eq!(texts("οδός"), ["8:ΟΔΌΣ"]);
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
fn text_with_spaces_around_it_still_shows_where_it_is() {
    let folder = folder(&[("a.rs", b"    let x = 1;\nbuy milk \n")]);
    assert_eq!(grep(&folder, "    let").0, ["a.rs:1:let x = 1;"]);
    assert_eq!(grep(&folder, "milk ").0, ["a.rs:2:buy milk"]);
    assert_eq!(excerpt_of(b"    let x", "    let"), ("let x".into(), 0..3));
    assert_eq!(
        excerpt_of(b"buy milk \n", "milk "),
        ("buy milk".into(), 4..8)
    );
    assert_eq!(excerpt_of(b"  a  b", "  "), ("a  b".into(), 1..3));
    // Only spaces that are trimmed away: nothing left to point at.
    assert_eq!(excerpt_of(b"    x", "  "), ("x".into(), 0..0));
}

#[test]
fn reads_line_endings_and_other_encodings() {
    let utf16 = |bom: &[u8], text: &str, big: bool| -> Vec<u8> {
        let mut bytes = bom.to_vec();
        for unit in text.encode_utf16() {
            bytes.extend(if big {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            });
        }
        bytes
    };
    let text = "first\r\nsecond milk\r\n";
    let folder = folder(&[
        ("dos.txt", b"\tmilk\r\nno\r\n"),
        ("bom8.txt", b"\xef\xbb\xbfmilk\n"),
        ("le.txt", &utf16(&[0xFF, 0xFE], text, false)),
        ("be.txt", &utf16(&[0xFE, 0xFF], text, true)),
        ("no-bom.txt", &utf16(&[], text, false)),
    ]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(
        lines,
        [
            "be.txt:2:second milk",
            "bom8.txt:1:milk",
            "dos.txt:1:milk",
            "le.txt:2:second milk",
        ]
    );
    // UTF-16 without its mark is full of zero bytes.
    assert_eq!(report.binary, 1);
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

/// A zero byte is found a block at a time: in the first block it skips the whole file,
/// further on it only ends the file, and lines already shown stay counted as searched.
#[test]
fn a_late_zero_byte_ends_the_file_after_its_lines() {
    let mut early = b"milk\n".to_vec();
    early.extend(b"x\0\n");
    let mut late = b"milk\n".to_vec();
    late.extend(b"line\n".repeat(100_000));
    late.extend(b"x\0\nmilk\n");
    let folder = folder(&[("early.log", &early), ("late.log", &late)]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(lines, ["late.log:1:milk"]);
    assert_eq!((report.files_searched, report.binary), (1, 1));
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
    let mut long_but_held = b"milk ".to_vec();
    long_but_held.resize(MAX_LINE_BYTES - 10, b'x');
    let mut after_a_match = b"milk\n".to_vec();
    after_a_match.extend(&minified);
    let folder = folder(&[
        ("bundle.min.js", &minified),
        ("held.js", &long_but_held),
        ("after.js", &after_a_match),
        ("a.txt", b"milk\n"),
    ]);
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(
        lines,
        [
            "a.txt:1:milk",
            "after.js:1:milk",
            &format!("held.js:1:milk {}", "x".repeat(195))
        ]
    );
    assert_eq!((report.files_searched, report.too_large), (3, 1));
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
    // A file nobody may open shows whether the check came first.
    let Some(_lock) = lock(&folder.root.join("cloud.txt")) else {
        return;
    };
    let mut grep = Grep::with_threads(2);
    grep.cloud_only = |path, _| path.ends_with("cloud.txt");
    let (lines, report) = grep_with(&grep, &folder, "milk", 10);
    assert_eq!(lines, ["local.txt:1:milk"]);
    assert_eq!((report.cloud_only, report.unreadable), (1, 0));
}

#[test]
fn a_file_moved_to_the_cloud_mid_search_counts_as_cloud_only() {
    static CHECKS: AtomicUsize = AtomicUsize::new(0);
    let folder = folder(&[("evicted.txt", b"milk\n")]);
    // Opening fails, as reading an evicted file does with downloads off.
    let Some(_lock) = lock(&folder.root.join("evicted.txt")) else {
        return;
    };
    let mut grep = Grep::with_threads(2);
    grep.cloud_only = |_, _| CHECKS.fetch_add(1, Ordering::Relaxed) > 0;
    let (_, report) = grep_with(&grep, &folder, "milk", 10);
    assert_eq!((report.cloud_only, report.unreadable), (1, 0));
}

#[test]
fn counts_files_it_may_not_open() {
    let folder = folder(&[("secret.txt", b"milk\n"), ("a.txt", b"milk\n")]);
    let Some(_lock) = lock(&folder.root.join("secret.txt")) else {
        return;
    };
    let (lines, report) = grep(&folder, "milk");
    assert_eq!(lines, ["a.txt:1:milk"]);
    assert_eq!(report.unreadable, 1);
}

#[test]
fn stops_at_the_limit() {
    let folder = many(40, |i| format!("{i}.txt"), b"milk\nmilk\nmilk\n");
    let grep = Grep::with_threads(4);
    for limit in [0, 1, 7, 50, 120, 500] {
        let (lines, report) = grep_with(&grep, &folder, "milk", limit);
        assert_eq!(lines.len(), limit.min(120));
        // Each thread finishes at most the file it is in, plus one it was about to open.
        let most = (limit.div_ceil(3) + 2 * grep.threads()).min(40);
        assert!(report.files_searched <= most, "{limit}: {report:?}");
    }
    assert_eq!(grep_with(&grep, &folder, "milk", 0).1, Report::default());
}

#[test]
fn a_newer_ticket_cancels_an_older_search() {
    let folder = folder(&[("a.txt", b"milk\n")]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let old = grep.ticket();
    let _new = grep.ticket();
    assert_eq!(grep.newest(), 2);
    assert_eq!(grep.search(&index, "milk", old, 10, |_| panic!()), None);
    // Given up before anything else, even text that would find nothing anyway.
    assert_eq!(grep.search(&index, "", old, 10, |_| panic!()), None);
}

#[test]
fn a_replaced_search_hands_over_no_more_lines() {
    let limit = usize::try_from(MAX_FILE_BYTES).unwrap();
    let mut big = b"milk and more words to make a longer line\n".repeat(limit / 43);
    big.truncate(limit);
    let folder = folder(&[("big.txt", &big)]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let lines = AtomicUsize::new(0);
    let result = grep.search(&index, "milk", grep.ticket(), usize::MAX, |_| {
        if lines.fetch_add(1, Ordering::Relaxed) == 0 {
            grep.ticket();
        }
    });
    assert_eq!(result, None);
    assert_eq!(lines.load(Ordering::Relaxed), 1);
}

#[test]
fn a_replaced_search_opens_no_more_files() {
    static OPENED: AtomicUsize = AtomicUsize::new(0);
    let folder = many(2000, |i| format!("d{}/{i}.txt", i % 20), b"milk\n");
    let index = index(&folder);
    let mut grep = Grep::with_threads(4);
    grep.cloud_only = |_, _| {
        OPENED.fetch_add(1, Ordering::Relaxed);
        false
    };
    let first = AtomicBool::new(true);
    let at_change = AtomicUsize::new(0);
    let result = grep.search(&index, "milk", grep.ticket(), usize::MAX, |_| {
        if first.swap(false, Ordering::Relaxed) {
            grep.ticket();
            at_change.store(OPENED.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    });
    assert_eq!(result, None);
    // Other threads may each be about to open one more file when the ticket changes.
    let opened = OPENED.load(Ordering::Relaxed);
    assert!(
        opened <= at_change.into_inner() + grep.threads(),
        "{opened}"
    );
}

#[test]
fn a_file_stops_at_the_next_read_once_the_search_is_over() {
    let folder = folder(&[("a.txt", b"milk\n")]);
    let matcher = Literal::new("milk").unwrap();
    let read = |stale: bool, limit: usize| {
        let stale = move || stale;
        let job = Job {
            matcher: &matcher,
            limit,
            delivered: AtomicUsize::new(0),
            report: Mutex::default(),
            found: &|_: Found<'_>| {},
            stale: &stale,
            cloud_only: cloud::is_cloud_only,
        };
        let file = File::open(folder.root.join("a.txt")).unwrap();
        Stoppable { file, job: &job }.read(&mut [0; 16]).unwrap()
    };
    assert_eq!(read(false, 1), 5);
    assert_eq!(read(true, 1), 0);
    assert_eq!(read(false, 0), 0);
}

#[test]
fn one_search_at_a_time() {
    let folder = folder(&[("a.txt", b"milk\n"), ("b.txt", b"milk\n")]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let (entered, older_entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let newer_lines = AtomicUsize::new(0);
    let (grep, index, newer_lines) = (&grep, &index, &newer_lines);
    std::thread::scope(|scope| {
        let ticket = grep.ticket();
        let (entered, released) = (&entered, &released);
        let older = scope.spawn(move || {
            grep.search(index, "milk", ticket, 10, |_| {
                let _ = entered.send(());
                let _ = released.lock().unwrap().recv();
            })
        });
        older_entered.recv().unwrap();
        let ticket = grep.ticket();
        let newer = scope.spawn(move || {
            grep.search(index, "milk", ticket, 10, |_| {
                newer_lines.fetch_add(1, Ordering::Relaxed);
            })
        });
        // A wrong pass shows within this pause; a right one never depends on it.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(newer_lines.load(Ordering::Relaxed), 0);
        drop(release);
        assert_eq!(older.join().unwrap(), None);
        assert_eq!(newer.join().unwrap().unwrap().lines_found, 2);
    });
}

#[test]
fn a_panic_in_the_callback_leaves_later_searches_working() {
    let folder = folder(&[("a.txt", b"milk\n")]);
    let index = index(&folder);
    let grep = Grep::with_threads(2);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        grep.search(&index, "milk", grep.ticket(), 10, |_| panic!("window gone"))
    }));
    assert!(panicked.is_err());
    assert_eq!(grep_with(&grep, &folder, "milk", 10).0, ["a.txt:1:milk"]);
}

#[cfg(target_os = "macos")]
#[test]
fn search_threads_never_download_and_yield_to_names() {
    let grep = Grep::with_threads(3);
    let each = grep
        .pool
        .broadcast(|_| (cloud::downloads_off(), priority::lowered()));
    assert_eq!(each, [(true, true); 3]);
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

    let (text, matched) = excerpt_of(format!("milk{}", "x".repeat(1000)).as_bytes(), "milk");
    assert_eq!((text.len(), matched), (MAX_TEXT_BYTES, 0..4));
}

#[test]
fn a_line_is_cut_only_past_the_limit() {
    let at_limit = format!("milk{}", "x".repeat(MAX_TEXT_BYTES - 4));
    assert_eq!(excerpt_of(at_limit.as_bytes(), "milk").0, at_limit);
    let over = format!("{}milk", "x".repeat(MAX_TEXT_BYTES - 3));
    let (text, matched) = excerpt_of(over.as_bytes(), "milk");
    assert_eq!((text.len(), &text[matched]), (MAX_TEXT_BYTES, "milk"));
}

#[test]
fn a_match_longer_than_the_cut_is_shown_up_to_the_cut() {
    let text = "m".repeat(MAX_TEXT_BYTES);
    let line = format!("{}{text}", "x".repeat(300));
    let (shown, matched) = excerpt_of(line.as_bytes(), &text);
    assert_eq!(shown.len(), MAX_TEXT_BYTES);
    assert_eq!(matched, BEFORE_MATCH..MAX_TEXT_BYTES);
}

#[test]
fn a_line_with_broken_letters_still_points_at_the_match() {
    let (text, matched) = excerpt_of(b"\xff\xfe caf\xc3\xa9 milk", "milk");
    assert_eq!(text, "\u{fffd}\u{fffd} café milk");
    assert_eq!(&text[matched], "milk");

    let mut line = b"\xff\xe2\x82".repeat(100);
    line.extend("ÉCOLE".as_bytes());
    line.extend(b"\x80".repeat(300));
    let (text, matched) = excerpt_of(&line, "école");
    assert_eq!(&text[matched], "ÉCOLE");
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

/// Whether a line holds the text, the slow and obvious way.
fn plainly_holds(line: &[u8], text: &str) -> bool {
    let has_case = |letter: char| letter.is_lowercase() || letter.is_uppercase();
    if text.chars().any(char::is_uppercase) || !text.chars().any(has_case) {
        return line
            .windows(text.len())
            .any(|window| window == text.as_bytes());
    }
    let fold = |letter| if letter == 'ς' { 'σ' } else { letter };
    let wanted: Vec<char> = text.to_lowercase().chars().map(fold).collect();
    let letters: Vec<Option<char>> = String::from_utf8_lossy(line)
        .chars()
        .map(|letter| {
            let mut lower = letter.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(lower), None) if letter.is_ascii() || !lower.is_ascii() => Some(fold(lower)),
                _ => None,
            }
        })
        .collect();
    letters.windows(wanted.len()).any(|window| {
        window
            .iter()
            .zip(&wanted)
            .all(|(&got, &want)| got == Some(want))
    })
}

/// Random small folders and random text: the search finds exactly the lines a plain
/// line-by-line check finds, and every file lands in exactly one count.
#[test]
fn any_search_agrees_with_a_plain_line_by_line_check() {
    let pieces = [
        "a", "A", "é", "É", "ß", "ẞ", "σ", "ς", "Σ", "k", "\u{212a}", " ", "\t", "\r", "\r\n",
        "\n", "\n", "\u{ff}", "\u{80}",
    ];
    let mut random = random_below();
    for _ in 0..300 {
        let files: Vec<(String, Vec<u8>)> = (0..=random(3))
            .map(|i| {
                let mut bytes = Vec::new();
                for _ in 0..random(30) {
                    match pieces[random(pieces.len())] {
                        // Stray bytes, not the letters themselves.
                        "\u{ff}" => bytes.push(0xFF),
                        "\u{80}" => bytes.push(0x80),
                        piece => bytes.extend(piece.as_bytes()),
                    }
                }
                (format!("{i}.txt"), bytes)
            })
            .collect();
        let text: String = (0..=random(3)).map(|_| pieces[random(11)]).collect();
        let named: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (n.as_str(), &b[..])).collect();
        let folder = folder(&named);
        let mut expected: Vec<String> = files
            .iter()
            .flat_map(|(name, bytes)| {
                let lines = bytes.split(|&byte| byte == b'\n').enumerate();
                lines
                    .filter(|(_, line)| plainly_holds(line, &text))
                    .map(move |(at, line)| {
                        let shown = String::from_utf8_lossy(line.trim_ascii());
                        format!("{name}:{}:{shown}", at + 1)
                    })
            })
            .collect();
        expected.sort();
        let (lines, report) = grep(&folder, &text);
        assert_eq!(lines, expected, "{text:?} in {files:?}");
        let counted = report.files_searched
            + report.binary
            + report.too_large
            + report.cloud_only
            + report.unreadable;
        assert_eq!((report.files_searched, counted), (files.len(), files.len()));
    }
}
