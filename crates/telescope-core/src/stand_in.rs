//! Made-up file names, so a window can be built and timed before it searches a real disk.

use crate::index::{Index, IndexBuilder, NO_PARENT};
use std::collections::HashSet;

const WORDS: &[&str] = &[
    "budget",
    "invoice",
    "report",
    "notes",
    "draft",
    "design",
    "server",
    "client",
    "photo",
    "telescope",
    "plan",
    "config",
    "button",
    "search",
    "panel",
    "window",
    "engine",
    "bridge",
    "holiday",
    "tax",
    "resume",
    "meeting",
    "roadmap",
    "slides",
    "archive",
    "backup",
    "café",
    "会議",
    "週報",
    "报告",
];
const EXTENSIONS: &[&str] = &[
    "md", "pdf", "rs", "swift", "tsx", "png", "jpg", "json", "txt", "docx", "xlsx",
];
/// Names that repeat across folders, as README.md and index.ts do on a real disk.
const COMMON: &[&str] = &[
    "README.md",
    "index.ts",
    "main.rs",
    "package.json",
    "Cargo.toml",
    ".gitignore",
];
/// About one entry in nine is a folder in your home folder.
const ENTRIES_PER_FOLDER: usize = 9;

/// `count` entries (at least the root) under a made-up `/stand-in` folder, the same every time.
pub fn stand_in(count: usize) -> Index {
    let mut builder = IndexBuilder::new();
    let mut folders = vec![builder.push("/stand-in", NO_PARENT, true)];
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    // Folders come first, so each one's parent is already in the index. A real folder never
    // holds two entries with the same name, or the window would show identical rows.
    let mut taken = HashSet::new();
    while folders.len() < (count / ENTRIES_PER_FOLDER).max(1) {
        let (word, other) = (random() % WORDS.len(), random() % WORDS.len());
        let parent = folders[random() % folders.len()];
        if taken.insert((parent, word, other)) {
            let name = format!("{}-{}", WORDS[word], WORDS[other]);
            folders.push(builder.push(&name, parent, true));
        }
    }
    let files = count.saturating_sub(folders.len());
    let mut names = Vec::new();
    for (k, &parent) in folders.iter().enumerate() {
        let wanted = files * (k + 1) / folders.len() - files * k / folders.len();
        names.clear();
        while names.len() < wanted {
            let (word, other) = (WORDS[random() % WORDS.len()], WORDS[random() % WORDS.len()]);
            let extension = EXTENSIONS[random() % EXTENSIONS.len()];
            let name = match random() % 4 {
                0 => COMMON[random() % COMMON.len()].to_owned(),
                1 => format!("{word}-{other}.{extension}"),
                _ => format!("{word}_{other}_{}.{extension}", random() % 100),
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for name in &names {
            builder.push(name, parent, false);
        }
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Searcher;

    #[test]
    fn the_same_names_every_time() {
        let (a, b) = (stand_in(5_000), stand_in(5_000));
        assert_eq!(a.len(), 5_000);
        assert_eq!(a.folder_count(), 5_000 / ENTRIES_PER_FOLDER);
        assert!(a.distinct_names() < a.len());
        assert!((0..5_000).all(|entry| a.path(entry) == b.path(entry)));
    }

    #[test]
    fn no_folder_holds_two_entries_with_the_same_name() {
        let index = stand_in(20_000);
        let paths: HashSet<_> = (0..20_000).map(|entry| index.path(entry)).collect();
        assert_eq!(paths.len(), 20_000);
        assert_eq!(stand_in(0).len(), 1);
        assert_eq!(stand_in(5).len(), 5);
    }

    #[test]
    fn made_up_names_can_be_found() {
        let index = stand_in(5_000);
        let searcher = Searcher::with_threads(2);
        let hits = searcher
            .search(
                &index,
                &crate::Apps::default(),
                "budget",
                searcher.ticket(),
                50,
            )
            .unwrap();
        assert_eq!(hits.len(), 50);
    }
}
