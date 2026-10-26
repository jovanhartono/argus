//! Search: the letter check, fuzzy matching per word, and dropping searches nobody wants any more.

use crate::index::{EntryId, Index, NO_PARENT, NameId, letters};
use frizbee::{Config, Matcher, Matching, Pattern, SortStrategy};
use rayon::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Each chunk is one job, and each job first checks that its search is still wanted.
const NAME_CHUNK: usize = 16_384;
const ENTRY_CHUNK: usize = 65_536;
/// One byte holds which words a name matched.
const MAX_WORDS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub entry: EntryId,
    pub score: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket(u64);

pub struct Searcher {
    pool: rayon::ThreadPool,
    newest: AtomicU64,
    /// One search at a time, reusing the same working lists, so a burst of keystrokes
    /// never holds several searches' memory and typing allocates nothing large.
    turn: Mutex<Scratch>,
}

#[derive(Default)]
struct Scratch {
    name_words: Vec<u8>,
    name_score: Vec<u16>,
    folder_words: Vec<u8>,
}

impl Default for Searcher {
    /// One thread per core minus one, so the window always has a core free.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self::with_threads((cores - 1).max(1))
    }
}

impl Searcher {
    pub fn with_threads(threads: usize) -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("search-{i}"))
            .build()
            .expect("start search threads");
        Self {
            pool,
            newest: AtomicU64::new(0),
            turn: Mutex::new(Scratch::default()),
        }
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Take one per keystroke. Every search holding an older ticket gives up.
    pub fn ticket(&self) -> Ticket {
        Ticket(self.newest.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// The best `limit` hits, or `None` when a newer ticket arrived first.
    pub fn search(
        &self,
        index: &Index,
        query: &str,
        ticket: Ticket,
        limit: usize,
    ) -> Option<Vec<Hit>> {
        let stale = || self.newest.load(Ordering::Relaxed) != ticket.0;
        let mut turn = self.turn.lock().unwrap();
        if stale() {
            return None;
        }
        let scratch = &mut *turn;
        self.pool.install(|| {
            let hits = run(index, query, limit, scratch, &stale)?;
            (!stale()).then_some(hits)
        })
    }
}

fn run(
    index: &Index,
    query: &str,
    limit: usize,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<Vec<Hit>> {
    let mut parsed = Pattern::parse_query(query);
    // Past eight words, extra search words are dropped before any exclusion.
    parsed.sort_by_key(|word| !word.negated);
    parsed.truncate(MAX_WORDS);
    let mut words = Vec::with_capacity(parsed.len());
    let (mut wanted, mut unwanted, mut fuzzy) = (0u8, 0u8, 0u8);
    for (w, word) in parsed.into_iter().enumerate() {
        if word.negated {
            unwanted |= 1 << w;
        } else {
            wanted |= 1 << w;
        }
        let word = positive(word);
        if word.config.matching.is_none() {
            fuzzy |= 1 << w;
        }
        words.push(word);
    }
    let limit = limit.min(index.len());
    if wanted == 0 || limit == 0 {
        return Some(Vec::new());
    }

    let mut matched = match_names(index, &words, wanted, 0, scratch, stale)?;
    // A search word that matches no name at all is probably misspelled, so it may take one typo.
    // Only fuzzy words can; `^`, `$` and `'` words match letter for letter.
    let missing = wanted & !matched;
    if missing != 0 && missing & !fuzzy == 0 {
        matched = match_names(index, &words, wanted, missing, scratch, stale)?;
    }
    if wanted & !matched != 0 {
        return Some(Vec::new());
    }
    let use_folders = wanted.count_ones() > 1 || unwanted != 0;
    if use_folders {
        words_above(index, scratch, stale)?;
    }

    let Scratch {
        name_words,
        name_score,
        folder_words,
    } = scratch;
    let chunks: Vec<Vec<Hit>> = index
        .entry_name
        .par_chunks(ENTRY_CHUNK)
        .zip(index.entry_parent.par_chunks(ENTRY_CHUNK))
        .enumerate()
        .map(|(chunk, (names, parents))| {
            if stale() {
                return None;
            }
            let first = chunk * ENTRY_CHUNK;
            let mut hits = Vec::with_capacity((2 * limit).min(names.len()));
            // Once `limit` hits are kept, anything scoring no better than the weakest is skipped,
            // so a one-letter search does not collect two million hits to keep 50.
            let mut floor = 0;
            for (i, (&name, &parent)) in names.iter().zip(parents).enumerate() {
                let own = name_words[name as usize];
                let score = u32::from(name_score[name as usize]);
                if own & wanted == 0
                    || parent == NO_PARENT
                    || (hits.len() >= limit && score <= floor)
                {
                    continue;
                }
                let mut path = own;
                if use_folders {
                    path |= folder_words[parent as usize];
                }
                if path & wanted == wanted && path & unwanted == 0 {
                    hits.push(Hit {
                        entry: (first + i) as EntryId,
                        score,
                    });
                    if hits.len() == 2 * limit {
                        keep_best(&mut hits, limit);
                        floor = hits[limit - 1].score;
                    }
                }
            }
            keep_best(&mut hits, limit);
            Some(hits)
        })
        .collect::<Option<_>>()?;

    let mut hits: Vec<Hit> = chunks.into_iter().flatten().collect();
    keep_best(&mut hits, limit);
    Some(hits)
}

/// Each distinct name is matched once, however many folders hold it.
/// Returns the words that matched at least one name.
fn match_names(
    index: &Index,
    words: &[Pattern],
    wanted: u8,
    typo_words: u8,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<u8> {
    let config = Config::default().sort(SortStrategy::IndexAsc);
    let typos = |w: usize| u16::from(typo_words >> w & 1);
    let needed: Vec<u64> = words.iter().map(|word| letters(&word.needle)).collect();
    scratch.name_words.clear();
    scratch.name_words.resize(index.distinct_names(), 0);
    scratch.name_score.clear();
    scratch.name_score.resize(index.distinct_names(), 0);

    scratch
        .name_words
        .par_chunks_mut(NAME_CHUNK)
        .zip(scratch.name_score.par_chunks_mut(NAME_CHUNK))
        .enumerate()
        .map_init(
            || {
                let matchers: Vec<Matcher> = words
                    .iter()
                    .enumerate()
                    .map(|(w, word)| Matcher::new(word.clone().max_typos(Some(typos(w))), &config))
                    .collect();
                (matchers, Vec::new(), Vec::new())
            },
            |(matchers, ids, texts), (chunk, (bits, scores))| {
                if stale() {
                    return None;
                }
                let first = chunk * NAME_CHUNK;
                let mut matched = 0u8;
                for (w, matcher) in matchers.iter_mut().enumerate() {
                    ids.clear();
                    texts.clear();
                    for n in first..first + bits.len() {
                        let absent = (needed[w] & !index.name_letters[n]).count_ones();
                        if absent <= u32::from(typos(w)) {
                            ids.push(n - first);
                            texts.push(index.name_text(n as NameId));
                        }
                    }
                    for found in matcher.match_list(texts) {
                        let n = ids[found.index as usize];
                        bits[n] |= 1 << w;
                        matched |= 1 << w;
                        if wanted >> w & 1 == 1 {
                            scores[n] = scores[n].saturating_add(found.score);
                        }
                    }
                }
                Some(matched)
            },
        )
        .try_reduce(|| 0, |a, b| Some(a | b))
}

/// The word as a pattern that finds matches, even when the word excludes them.
/// One letter matches the same names in order or anywhere, and the plain search is faster.
fn positive(word: Pattern) -> Pattern {
    let word = word.negated(false);
    if word.config.matching.is_none() && word.needle.chars().count() == 1 {
        word.matching(Some(Matching::Substring))
    } else {
        word
    }
}

/// One pass in entry order works because a folder is numbered before its contents.
/// The root's own name, the full path you scanned, never counts.
fn words_above(
    index: &Index,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<()> {
    let Scratch {
        name_words,
        folder_words,
        ..
    } = scratch;
    folder_words.clear();
    folder_words.resize(index.len(), 0);
    for (i, &folder) in index.folders.iter().enumerate() {
        if i % ENTRY_CHUNK == 0 && stale() {
            return None;
        }
        let folder = folder as usize;
        let parent = index.entry_parent[folder];
        if parent != NO_PARENT {
            let own = name_words[index.entry_name[folder] as usize];
            folder_words[folder] = own | folder_words[parent as usize];
        }
    }
    Some(())
}

fn keep_best(hits: &mut Vec<Hit>, limit: usize) {
    let order = |a: &Hit, b: &Hit| b.score.cmp(&a.score).then(a.entry.cmp(&b.entry));
    if hits.len() > limit {
        hits.select_nth_unstable_by(limit, order);
        hits.truncate(limit);
    }
    hits.sort_unstable_by(order);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexBuilder;

    fn sample() -> Index {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let synergy = builder.push("synergy-core", root, true);
        let other = builder.push("other", root, true);
        builder.push("README.md", synergy, false);
        builder.push("README.md", other, false);
        builder.push("plan.md", other, false);
        builder.finish()
    }

    fn paths(index: &Index, query: &str) -> Vec<String> {
        let searcher = Searcher::with_threads(2);
        let hits = searcher
            .search(index, query, searcher.ticket(), 10)
            .unwrap();
        let mut found: Vec<String> = hits
            .iter()
            .map(|hit| index.path(hit.entry).to_string_lossy().replace('\\', "/"))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn one_word_matches_names_only() {
        let index = sample();
        assert_eq!(
            paths(&index, "readme"),
            ["/home/other/README.md", "/home/synergy-core/README.md"]
        );
        assert!(paths(&index, "home").is_empty());
    }

    #[test]
    fn other_words_may_match_a_folder_above() {
        let index = sample();
        assert_eq!(
            paths(&index, "synergy readme"),
            ["/home/synergy-core/README.md"]
        );
        assert_eq!(paths(&index, "readme !synergy"), ["/home/other/README.md"]);
    }

    #[test]
    fn allows_one_typo_only_for_a_word_that_matches_nothing() {
        let index = sample();
        assert_eq!(paths(&index, "plna"), ["/home/other/plan.md"]);
        assert_eq!(paths(&index, "other plna"), ["/home/other/plan.md"]);
        assert!(paths(&index, "zzz").is_empty());
        assert!(paths(&index, "^plna").is_empty());
    }

    #[test]
    fn an_exclusion_never_turns_on_typos() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let synergy = builder.push("synergy", root, true);
        builder.push("plan.md", synergy, false);
        builder.push("pla.md", root, false);
        let index = builder.finish();
        assert_eq!(paths(&index, "plan"), ["/home/synergy/plan.md"]);
        assert!(paths(&index, "plan !synergy").is_empty());
    }

    #[test]
    fn a_newer_ticket_cancels_an_older_search() {
        let index = sample();
        let searcher = Searcher::with_threads(2);
        let old = searcher.ticket();
        let _new = searcher.ticket();
        assert_eq!(searcher.search(&index, "readme", old, 10), None);
    }

    #[test]
    fn a_search_gives_up_partway_when_replaced() {
        let index = sample();
        let checks = AtomicU64::new(0);
        // Still wanted at the first check, replaced by the second.
        let stale = || checks.fetch_add(1, Ordering::Relaxed) > 0;
        assert_eq!(
            run(&index, "readme", 10, &mut Scratch::default(), &stale),
            None
        );
        assert_eq!(checks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_small_limit_keeps_the_same_best_hits() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for i in 0..300 {
            let folder = builder.push(&format!("folder {i}"), root, true);
            builder.push(&format!("plan {}.md", i % 7), folder, false);
            builder.push(&format!("p{i}-l-a-n"), folder, false);
        }
        let index = builder.finish();
        let searcher = Searcher::with_threads(2);
        let all = searcher
            .search(&index, "plan", searcher.ticket(), 1000)
            .unwrap();
        let few = searcher
            .search(&index, "plan", searcher.ticket(), 5)
            .unwrap();
        assert_eq!(few, all[..5]);
    }

    #[test]
    fn searches_in_a_row_do_not_leak_into_each_other() {
        let index = sample();
        let searcher = Searcher::with_threads(2);
        searcher.search(&index, "synergy readme", searcher.ticket(), 10);
        let hits = searcher
            .search(&index, "plan", searcher.ticket(), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
    }
}
