//! Search: the letter check, fuzzy matching per word, and dropping searches nobody wants any more.

use crate::index::{EntryId, Index, NO_PARENT, NameId, letter, letters};
use frizbee::{Config, Matcher, Matching, Pattern, Scoring, SortStrategy};
use rayon::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Each chunk is one job, and each job first checks that its search is still wanted.
const NAME_CHUNK: usize = 16_384;
const ENTRY_CHUNK: usize = 65_536;
/// One byte holds which words a name matched.
const MAX_WORDS: usize = 8;
/// Ranking weights, in frizbee's units, where a matched letter is worth 12.
const WHOLE_WORD_BONUS: u16 = 24;
const DEPTH_PENALTY: u32 = 2;
const RARER_BY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub entry: EntryId,
    pub score: u32,
    name_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket(u64);

impl Ticket {
    /// The ticket as a plain number, for a window on the other side of the hatch.
    pub fn number(self) -> u64 {
        self.0
    }
}

/// One ticket per keystroke; only the newest ticket's search is still wanted.
#[derive(Debug, Default)]
pub struct Tickets(AtomicU64);

impl Tickets {
    pub fn take(&self) -> Ticket {
        Ticket(self.0.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// The number of the last ticket handed out.
    pub fn newest(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn is_stale(&self, ticket: Ticket) -> bool {
        self.newest() != ticket.0
    }
}

pub struct Searcher {
    pool: rayon::ThreadPool,
    tickets: Tickets,
    /// One search at a time, reusing the same working lists, so a burst of keystrokes
    /// never holds several searches' memory and typing allocates nothing large.
    turn: Mutex<Scratch>,
}

#[derive(Default)]
struct Scratch {
    name_words: Vec<u8>,
    name_score: Vec<u16>,
    folder_words: Vec<u8>,
    /// One bit per distinct name a result could still need, for matching common words last.
    possible: Vec<AtomicU64>,
    /// One bit per folder already climbed while marking `possible`.
    walked: Vec<AtomicU64>,
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
            tickets: Tickets::default(),
            turn: Mutex::new(Scratch::default()),
        }
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Take one per keystroke. Every search holding an older ticket gives up.
    pub fn ticket(&self) -> Ticket {
        self.tickets.take()
    }

    /// The number of the last ticket handed out; only its search is still wanted.
    pub fn newest(&self) -> u64 {
        self.tickets.newest()
    }

    /// The best `limit` hits, or `None` when a newer ticket arrived first.
    pub fn search(
        &self,
        index: &Index,
        query: &str,
        ticket: Ticket,
        limit: usize,
    ) -> Option<Vec<Hit>> {
        let stale = || self.tickets.is_stale(ticket);
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
    let (mut wanted, mut unwanted, mut typo_ready) = (0u8, 0u8, 0u8);
    for (w, word) in parsed.into_iter().enumerate() {
        if word.negated {
            unwanted |= 1 << w;
        } else {
            wanted |= 1 << w;
        }
        let word = positive(word);
        // Below four letters one typo matches almost anything, and `^`, `$` and `'` words
        // always match letter for letter.
        if word.config.matching.is_none() && word.needle.chars().count() >= 4 {
            typo_ready |= 1 << w;
        }
        words.push(word);
    }
    let limit = limit.min(index.len());
    if wanted == 0 || limit == 0 {
        return Some(Vec::new());
    }

    let later = common_words(index, &words, wanted, typo_ready);
    // "38" fits a third of all names, mostly hashes that only scatter its letters. Names where
    // it starts the name or a word go first, and the rest only when those cannot fill the list.
    let ceiling = match words.as_slice() {
        [word] => plain_ceiling(word, &matcher_config().scoring),
        _ => None,
    };
    let (mut matched, held) = match_names(
        index,
        &words,
        !later,
        wanted,
        if ceiling.is_some() {
            Pass::Strong
        } else {
            Pass::Everywhere { typos: 0 }
        },
        scratch,
        stale,
    )?;
    // The file you want almost always holds each word you type, letter for letter. A word no
    // name holds is probably misspelled, like "desing", so it may take one typo.
    let unsure = wanted & typo_ready & !held;
    if unsure != 0 && wanted & !later & !matched & !unsure == 0 {
        (matched, _) = match_names(
            index,
            &words,
            !later,
            wanted,
            Pass::Everywhere { typos: unsure },
            scratch,
            stale,
        )?;
    }
    if later != 0 && wanted & !later & !matched == 0 {
        // Folder words without the common ones yet; they are worked out again below.
        words_above(index, scratch, stale)?;
        mark_possible(index, wanted & !later, unwanted, scratch, stale)?;
        matched |= match_names(index, &words, later, wanted, Pass::Possible, scratch, stale)?.0;
    }
    // A word found in a folder above counts as a clean match of that word, so "synergy readme"
    // ranks synergy-core/README.md above a long name that scatters the letters of both words.
    let letter = u32::from(Config::default().scoring.match_score);
    let mut folder_bonus = [0u32; 1 << MAX_WORDS];
    for (mask, bonus) in folder_bonus.iter_mut().enumerate() {
        for (w, word) in words.iter().enumerate() {
            if mask >> w & 1 == 1 {
                *bonus += letter * word.needle.chars().count() as u32;
            }
        }
    }
    if let Some(ceiling) = ceiling {
        let hits = best(
            index,
            scratch,
            wanted,
            unwanted,
            &folder_bonus,
            limit,
            stale,
        )?;
        // Every entry sits at least one folder deep, which costs a plain name a little more.
        let plain_best = u32::from(ceiling).saturating_sub(DEPTH_PENALTY);
        if hits.len() == limit && hits[limit - 1].score > plain_best {
            return Some(hits);
        }
        matched |= match_names(index, &words, wanted, wanted, Pass::Rest, scratch, stale)?.0;
    }
    if wanted & !matched != 0 {
        return Some(Vec::new());
    }
    if uses_folders(wanted, unwanted) {
        words_above(index, scratch, stale)?;
    }
    best(
        index,
        scratch,
        wanted,
        unwanted,
        &folder_bonus,
        limit,
        stale,
    )
}

/// One word must be in the name itself; more words, or an exclusion, also look at the folders.
fn uses_folders(wanted: u8, unwanted: u8) -> bool {
    wanted.count_ones() > 1 || unwanted != 0
}

/// The best `limit` entries whose path holds every `wanted` word and no `unwanted` one.
fn best(
    index: &Index,
    scratch: &Scratch,
    wanted: u8,
    unwanted: u8,
    folder_bonus: &[u32; 1 << MAX_WORDS],
    limit: usize,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<Vec<Hit>> {
    let use_folders = uses_folders(wanted, unwanted);
    let Scratch {
        name_words,
        name_score,
        folder_words,
        ..
    } = scratch;
    let chunks: Vec<Vec<Hit>> = index
        .entry_name
        .par_chunks(ENTRY_CHUNK)
        .zip(index.entry_parent.par_chunks(ENTRY_CHUNK))
        .zip(index.entry_depth.par_chunks(ENTRY_CHUNK))
        .enumerate()
        .map(|(chunk, ((names, parents), depths))| {
            if stale() {
                return None;
            }
            let first = chunk * ENTRY_CHUNK;
            let mut hits = Vec::with_capacity((2 * limit).min(names.len()));
            // Once `limit` hits are kept, anything ranking below the weakest is skipped,
            // so a one-letter search does not collect two million hits to keep 50.
            let mut weakest: Option<Hit> = None;
            let entries = names.iter().zip(parents).zip(depths).enumerate();
            for (i, ((&name, &parent), &depth)) in entries {
                let own = name_words[name as usize];
                if own & wanted == 0 || parent == NO_PARENT {
                    continue;
                }
                let mut path = own;
                if use_folders {
                    path |= folder_words[parent as usize];
                }
                if path & wanted != wanted || path & unwanted != 0 {
                    continue;
                }
                let score = (u32::from(name_score[name as usize])
                    + folder_bonus[usize::from(wanted & !own)])
                .saturating_sub(DEPTH_PENALTY * u32::from(depth));
                if weakest.is_some_and(|weakest| score < weakest.score) {
                    continue;
                }
                let hit = Hit {
                    entry: (first + i) as EntryId,
                    score,
                    name_len: index.name_len(name),
                };
                if weakest.is_some_and(|weakest| order(&hit, &weakest).is_ge()) {
                    continue;
                }
                hits.push(hit);
                if hits.len() == 2 * limit {
                    keep_best(&mut hits, limit);
                    weakest = Some(hits[limit - 1]);
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

fn matcher_config() -> Config {
    let mut config = Config::default().sort(SortStrategy::IndexAsc);
    // Typing in lower case must not favour lower-case names over NIB.pdf.
    config.scoring.matching_case_bonus = 0;
    config
}

#[derive(Clone, Copy)]
enum Pass {
    /// Every name, starting over, with one typo allowed for the `typos` words.
    Everywhere { typos: u8 },
    /// Only the names where a letter of the word starts the name or one of its words, starting over.
    Strong,
    /// The other names, adding to what `Strong` found.
    Rest,
    /// Only the names marked possible, adding to what the earlier pass found.
    Possible,
}

/// Each distinct name is matched once, however many folders hold it, for the `active` words.
/// Returns the words that matched at least one name, and the search words some name holds.
fn match_names(
    index: &Index,
    words: &[Pattern],
    active: u8,
    wanted: u8,
    pass: Pass,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<(u8, u8)> {
    let config = matcher_config();
    let typo_words = match pass {
        Pass::Everywhere { typos } => typos,
        Pass::Strong | Pass::Rest | Pass::Possible => 0,
    };
    let typos = |w: usize| u16::from(typo_words >> w & 1);
    let needed: Vec<u64> = words.iter().map(|word| letters(&word.needle)).collect();
    let floors: Vec<u16> = (0..words.len())
        .map(|w| held_floor(&words[w], typos(w), &config.scoring))
        .collect();
    let one_letters: Vec<Option<OneLetter>> = (0..words.len())
        .map(|w| OneLetter::new(&words[w], typos(w)))
        .collect();
    let Scratch {
        name_words,
        name_score,
        possible,
        ..
    } = scratch;
    let only_possible = matches!(pass, Pass::Possible);
    if matches!(pass, Pass::Everywhere { .. } | Pass::Strong) {
        name_words.clear();
        name_words.resize(index.distinct_names(), 0);
        name_score.clear();
        name_score.resize(index.distinct_names(), 0);
    }
    let possible = &*possible;

    name_words
        .par_chunks_mut(NAME_CHUNK)
        .zip(name_score.par_chunks_mut(NAME_CHUNK))
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
                let (mut matched, mut held) = (0u8, 0u8);
                for (w, matcher) in matchers.iter_mut().enumerate() {
                    if active >> w & 1 == 0 {
                        continue;
                    }
                    ids.clear();
                    texts.clear();
                    // "zshrc" should find .zshrc as readily as zshrc.bak; typing ".zsh" keeps the dot.
                    let keep_dot = words[w].needle.starts_with('.');
                    for n in first..first + bits.len() {
                        if only_possible && !is_marked(possible, n) {
                            continue;
                        }
                        let have = index.name_letters[n];
                        if (needed[w] & !have).count_ones() > u32::from(typos(w)) {
                            continue;
                        }
                        let starts = || index.name_word_starts[n];
                        let plain = || starts() & needed[w] == 0;
                        let skip = match pass {
                            Pass::Strong => plain(),
                            Pass::Rest => !plain(),
                            Pass::Everywhere { .. } | Pass::Possible => false,
                        };
                        if skip {
                            continue;
                        }
                        if let Some(letter) = one_letters[w]
                            && have != u64::MAX
                        {
                            bits[n - first] |= 1 << w;
                            matched |= 1 << w;
                            if wanted >> w & 1 == 1 {
                                let text = || index.name_text(n as NameId);
                                let score = letter.score(starts(), text, &config.scoring);
                                scores[n - first] = scores[n - first].saturating_add(score);
                            }
                            continue;
                        }
                        let text = index.name_text(n as NameId);
                        ids.push(n - first);
                        texts.push(match text.strip_prefix('.') {
                            Some(hidden) if !keep_dot => hidden,
                            _ => text,
                        });
                    }
                    for found in matcher.match_list(texts) {
                        let n = ids[found.index as usize];
                        bits[n] |= 1 << w;
                        matched |= 1 << w;
                        if wanted >> w & 1 == 1 {
                            let mut score = found.score;
                            // Most names matching "38" are hashes that scatter the letters.
                            let how = if score < floors[w] {
                                Held::No
                            } else {
                                held_in(texts[found.index as usize], &words[w].needle)
                            };
                            match how {
                                how @ (Held::Whole | Held::Alone) => {
                                    if matches!(how, Held::Whole) && !found.exact {
                                        score =
                                            score.saturating_add(config.scoring.exact_match_bonus);
                                    }
                                    score = score.saturating_add(WHOLE_WORD_BONUS);
                                    held |= 1 << w;
                                }
                                Held::Inside => held |= 1 << w,
                                Held::No => {}
                            }
                            scores[n] = scores[n].saturating_add(score);
                        }
                    }
                }
                Some((matched, held))
            },
        )
        .try_reduce(|| (0, 0), |a, b| Some((a.0 | b.0, a.1 | b.1)))
}

/// How a name holds a typed word, letter for letter: "nib" is the whole of "NIB.pdf" bar its
/// extension, stands alone in "my NIB.pdf", sits inside "nibble.go", and is not held by "nimble.go".
enum Held {
    No,
    Inside,
    Alone,
    Whole,
}

fn held_in(name: &str, word: &str) -> Held {
    let (name, word) = (name.as_bytes(), word.as_bytes());
    let alphanumeric = |byte: Option<&u8>| byte.is_some_and(u8::is_ascii_alphanumeric);
    let mut result = Held::No;
    // One letter matches most names; checking each for it is time with nothing to gain.
    if word.len() < 2 || word.len() > name.len() {
        return result;
    }
    for (i, window) in name.windows(word.len()).enumerate() {
        if window.eq_ignore_ascii_case(word) {
            if !alphanumeric(name[..i].last()) && !alphanumeric(name.get(i + word.len())) {
                return match &name[i + word.len()..] {
                    [] if i == 0 => Held::Whole,
                    [b'.', extension @ ..] if i == 0 && !extension.contains(&b'.') => Held::Whole,
                    _ => Held::Alone,
                };
            }
            result = Held::Inside;
        }
    }
    result
}

/// Below this score a name cannot hold the word letter for letter, as frizbee gives each letter
/// of an unbroken run at least `match_score`. Only when it ignores case and allows no typo.
fn held_floor(word: &Pattern, typos: u16, scoring: &Scoring) -> u16 {
    let ignores_case =
        word.needle.is_ascii() && !word.needle.bytes().any(|b| b.is_ascii_uppercase());
    if typos > 0 || !ignores_case {
        return 0;
    }
    u16::try_from(word.needle.len()).map_or(0, |len| len.saturating_mul(scoring.match_score))
}

/// The most a name scores for a short lower-case word when no letter of it starts the name
/// or a word: one `match_score` per letter, since only those starts earn bonuses.
fn plain_ceiling(word: &Pattern, scoring: &Scoring) -> Option<u16> {
    let bytes = word.needle.as_bytes();
    let simple = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let fuzzy_or_inside = matches!(word.config.matching, None | Some(Matching::Substring));
    (fuzzy_or_inside && (1..=3).contains(&bytes.len()) && bytes.iter().all(simple))
        .then(|| scoring.match_score * bytes.len() as u16)
}

/// One lower-case letter or digit, scored from the fingerprints exactly as frizbee would score
/// it, because "c" alone fits two thirds of all names and frizbee would read each one.
#[derive(Clone, Copy)]
struct OneLetter {
    byte: u8,
    bit: u64,
}

impl OneLetter {
    fn new(word: &Pattern, typos: u16) -> Option<Self> {
        match *word.needle.as_bytes() {
            [byte]
                if typos == 0
                    && word.config.matching == Some(Matching::Substring)
                    && (byte.is_ascii_lowercase() || byte.is_ascii_digit()) =>
            {
                Some(Self {
                    byte,
                    bit: letter(byte),
                })
            }
            _ => None,
        }
    }

    /// The score of a name that holds the letter. Only a letter that starts the name or a word
    /// needs the name read; starting the name earns the most.
    fn score<'a>(self, starts: u64, name: impl FnOnce() -> &'a str, scoring: &Scoring) -> u16 {
        if starts & self.bit == 0 {
            return scoring.match_score;
        }
        let name = name();
        let name = name.strip_prefix('.').unwrap_or(name).as_bytes();
        if name
            .first()
            .is_some_and(|b| b.to_ascii_lowercase() == self.byte)
        {
            let first = scoring.match_score.saturating_add(scoring.prefix_bonus);
            if name.len() == 1 {
                return first.saturating_add(scoring.exact_match_bonus);
            }
            return first;
        }
        scoring.match_score.saturating_add(scoring.delimiter_bonus)
    }
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

/// Short words like the "a" in "cv a" fit most names. Matched last, only on names a result
/// could still need, they find the same results for a fraction of the work.
/// A word that may take a typo is never put off, as its typo depends on every name.
fn common_words(index: &Index, words: &[Pattern], wanted: u8, typo_ready: u8) -> u8 {
    if wanted.count_ones() < 2 || wanted & !typo_ready == 0 {
        return 0;
    }
    let fitting: Vec<usize> = (0..words.len())
        .map(|w| {
            if wanted >> w & 1 == 0 {
                return usize::MAX;
            }
            let needed = letters(&words[w].needle);
            let names = index.name_letters.par_iter();
            names.filter(|&&name| needed & !name == 0).count()
        })
        .collect();
    let rarest = fitting.iter().copied().min().unwrap_or(0);
    // Unless a word fits far more names than the rarest, the extra passes cost more than they save.
    let mut later = 0;
    for (w, &fits) in fitting.iter().enumerate() {
        if (wanted & !typo_ready) >> w & 1 == 1 && fits > RARER_BY * rarest {
            later |= 1 << w;
        }
    }
    later
}

/// Marks the names a result could still need: each entry whose path holds every `rare` word
/// and no `unwanted` one, and each folder above one.
fn mark_possible(
    index: &Index,
    rare: u8,
    unwanted: u8,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<()> {
    let Scratch {
        name_words,
        folder_words,
        possible,
        walked,
        ..
    } = scratch;
    reset(possible, index.distinct_names());
    reset(walked, index.len());
    let (possible, walked) = (&*possible, &*walked);
    index
        .entry_name
        .par_chunks(ENTRY_CHUNK)
        .zip(index.entry_parent.par_chunks(ENTRY_CHUNK))
        .try_for_each(|(names, parents)| {
            if stale() {
                return None;
            }
            for (&name, &parent) in names.iter().zip(parents) {
                if parent == NO_PARENT {
                    continue;
                }
                let path = name_words[name as usize] | folder_words[parent as usize];
                if path & rare != rare || path & unwanted != 0 {
                    continue;
                }
                mark(possible, name as usize);
                let mut folder = parent;
                // A folder someone already walked through has its own folders above marked.
                while folder != NO_PARENT && !mark(walked, folder as usize) {
                    mark(possible, index.entry_name[folder as usize] as usize);
                    folder = index.entry_parent[folder as usize];
                }
            }
            Some(())
        })
}

fn reset(bits: &mut Vec<AtomicU64>, len: usize) {
    bits.clear();
    bits.resize_with(len.div_ceil(64), AtomicU64::default);
}

/// Sets bit `i`, and says whether it was set already.
fn mark(bits: &[AtomicU64], i: usize) -> bool {
    let bit = 1 << (i % 64);
    bits[i / 64].fetch_or(bit, Ordering::Relaxed) & bit != 0
}

fn is_marked(bits: &[AtomicU64], i: usize) -> bool {
    bits[i / 64].load(Ordering::Relaxed) >> (i % 64) & 1 == 1
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

/// Best first: the higher score, then the shorter name, which holds fewer letters you did not
/// type, then the entry number, so the result never depends on which thread finished first.
fn order(a: &Hit, b: &Hit) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then(a.name_len.cmp(&b.name_len))
        .then(a.entry.cmp(&b.entry))
}

fn keep_best(hits: &mut Vec<Hit>, limit: usize) {
    if hits.len() > limit {
        hits.select_nth_unstable_by(limit, order);
        hits.truncate(limit);
    }
    hits.sort_unstable_by(order);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexBuilder, word_starts};

    #[test]
    fn a_common_word_still_counts_in_any_folder_above() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let cv = builder.push("cv", root, true);
        builder.push("a.pdf", cv, false);
        builder.push("zz.txt", cv, false);
        let data = builder.push("data", root, true);
        let deep = builder.push("deep", data, true);
        builder.push("cv.txt", deep, false);
        builder.push("cv-plan.md", root, false);
        builder.push("notes.md", root, false);
        // Enough names holding "a" that it is matched last.
        for i in 0..20 {
            builder.push(&format!("bar{i}.txt"), root, false);
        }
        let index = builder.finish();
        assert_eq!(
            paths(&index, "cv a"),
            [
                "/home/cv-plan.md",
                "/home/cv/a.pdf",
                "/home/data/deep/cv.txt"
            ]
        );
    }

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

    fn first(index: &Index, query: &str) -> String {
        let searcher = Searcher::with_threads(2);
        let hits = searcher
            .search(index, query, searcher.ticket(), 10)
            .unwrap();
        index
            .path(hits[0].entry)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// A small xorshift, so the property tests need no extra crate and fail the same way twice.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        fn text(&mut self, alphabet: &[&str], max: usize) -> String {
            let len = self.below(max) + 1;
            (0..len)
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }
    }

    #[test]
    fn held_floor_never_hides_a_held_word() {
        let mut rng = Rng(0x5eed);
        let names = [
            "a", "b", "3", "8", "A", "B", ".", "-", "_", " ", "/", "(", "é", "a", "3",
        ];
        let lower = ["a", "b", "3", "8", ".", "_"];
        let mut config = Config::default().sort(SortStrategy::IndexAsc);
        config.scoring.matching_case_bonus = 0;
        let mut checked = 0;
        for _ in 0..3_000 {
            let typed = match rng.below(4) {
                0 => rng.text(&lower, 1).to_uppercase() + &rng.text(&lower, 3),
                _ => rng.text(&lower, 4),
            };
            let word = Pattern::parse_query(&typed).remove(0).max_typos(Some(0));
            assert_eq!(word.needle, typed);
            let floor = held_floor(&word, 0, &config.scoring);
            let haystacks: Vec<String> = (0..40).map(|_| rng.text(&names, 30)).collect();
            for found in Matcher::new(word.clone(), &config).match_list(&haystacks) {
                let name = &haystacks[found.index as usize];
                if !matches!(held_in(name, &word.needle), Held::No) {
                    checked += 1;
                    assert!(found.score >= floor, "{typed:?} in {name:?}");
                }
            }
        }
        assert!(checked > 1_000);
    }

    #[test]
    fn one_letter_scores_match_frizbee() {
        let mut rng = Rng(0xc0ffee);
        let names = [
            "a", "b", "c", "3", "A", "B", "C", ".", "-", "_", " ", "(", "~", "a", "c",
        ];
        let mut config = Config::default().sort(SortStrategy::IndexAsc);
        config.scoring.matching_case_bonus = 0;
        let mut checked = 0;
        for typed in ["a", "b", "c", "3", "z"] {
            let word = positive(Pattern::parse_query(typed).remove(0)).max_typos(Some(0));
            let letter = OneLetter::new(&word, 0).unwrap();
            let haystacks: Vec<String> = (0..20_000).map(|_| rng.text(&names, 12)).collect();
            let stripped: Vec<&str> = haystacks
                .iter()
                .map(|name| name.strip_prefix('.').unwrap_or(name))
                .collect();
            let mut frizbee = vec![None; haystacks.len()];
            for found in Matcher::new(word.clone(), &config).match_list(&stripped) {
                frizbee[found.index as usize] = Some(found.score);
            }
            for (name, expected) in haystacks.iter().zip(frizbee) {
                let ours = (letters(name) & letter.bit != 0)
                    .then(|| letter.score(word_starts(name), || name, &config.scoring));
                assert_eq!(ours, expected, "{typed:?} in {name:?}");
                checked += usize::from(ours.is_some());
            }
        }
        assert!(checked > 20_000);
    }

    #[test]
    fn a_plain_name_never_scores_above_the_ceiling() {
        let mut rng = Rng(0xfeed);
        let names = [
            "a", "b", "3", "8", "A", "B", ".", "-", "_", " ", "(", "é", "a", "3", "8",
        ];
        let lower = ["a", "b", "3", "8"];
        let mut config = Config::default().sort(SortStrategy::IndexAsc);
        config.scoring.matching_case_bonus = 0;
        let mut checked = 0;
        for _ in 0..2_000 {
            let typed = rng.text(&lower, 3);
            let word = positive(Pattern::parse_query(&typed).remove(0)).max_typos(Some(0));
            let ceiling = plain_ceiling(&word, &config.scoring).unwrap();
            let needed = letters(&typed);
            let haystacks: Vec<String> = (0..40).map(|_| rng.text(&names, 16)).collect();
            let stripped: Vec<&str> = haystacks
                .iter()
                .map(|name| name.strip_prefix('.').unwrap_or(name))
                .collect();
            for found in Matcher::new(word.clone(), &config).match_list(&stripped) {
                let name = &haystacks[found.index as usize];
                if word_starts(name) & needed == 0 {
                    checked += 1;
                    let held = held_in(stripped[found.index as usize], &typed);
                    assert!(found.score <= ceiling, "{typed:?} in {name:?}");
                    assert!(
                        matches!(held, Held::No | Held::Inside),
                        "{typed:?} in {name:?}"
                    );
                }
            }
        }
        assert!(checked > 5_000);
    }

    #[test]
    fn plain_ceiling_is_only_for_short_simple_words() {
        let scoring = Config::default().scoring;
        let word = |text: &str| positive(Pattern::parse_query(text).remove(0));
        assert_eq!(plain_ceiling(&word("c"), &scoring), Some(12));
        assert_eq!(plain_ceiling(&word("385"), &scoring), Some(36));
        assert_eq!(plain_ceiling(&word("3857"), &scoring), None);
        assert_eq!(plain_ceiling(&word("Cv"), &scoring), None);
        assert_eq!(plain_ceiling(&word("a.b"), &scoring), None);
        assert_eq!(plain_ceiling(&word("^cv"), &scoring), None);
    }

    #[test]
    fn every_limit_keeps_the_head_of_the_full_list() {
        let mut rng = Rng(0xbead);
        let parts = ["3", "8", "x", "y", "-", "X"];
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let mut folders = vec![root];
        for i in 0..400 {
            let parent = folders[rng.below(folders.len())];
            let name = rng.text(&parts, 7);
            let entry = builder.push(&format!("{name}{i}"), parent, i % 5 == 0);
            if i % 5 == 0 {
                folders.push(entry);
            }
        }
        let index = builder.finish();
        let searcher = Searcher::with_threads(2);
        for query in ["3", "38", "x8", "38x", "y"] {
            let all = searcher
                .search(&index, query, searcher.ticket(), 1_000)
                .unwrap();
            assert!(all.len() > 20, "{query:?}");
            for limit in 1..=all.len() {
                let few = searcher
                    .search(&index, query, searcher.ticket(), limit)
                    .unwrap();
                assert_eq!(few, all[..limit], "{query:?} limit {limit}");
            }
        }
    }

    #[test]
    fn only_plain_lower_case_letters_skip_the_matcher() {
        let word = |text: &str| positive(Pattern::parse_query(text).remove(0));
        assert!(OneLetter::new(&word("c"), 0).is_some());
        assert!(OneLetter::new(&word("7"), 0).is_some());
        assert!(OneLetter::new(&word("C"), 0).is_none());
        assert!(OneLetter::new(&word("."), 0).is_none());
        assert!(OneLetter::new(&word("é"), 0).is_none());
        assert!(OneLetter::new(&word("^c"), 0).is_none());
        assert!(OneLetter::new(&word("cv"), 0).is_none());
    }

    #[test]
    fn held_floor_is_off_for_typos_and_upper_case() {
        let scoring = Config::default().scoring;
        let word = |text: &str| Pattern::parse_query(text).remove(0);
        assert_eq!(held_floor(&word("38"), 0, &scoring), 24);
        assert_eq!(held_floor(&word("38"), 1, &scoring), 0);
        assert_eq!(held_floor(&word("Nib"), 0, &scoring), 0);
        assert_eq!(held_floor(&word("café"), 0, &scoring), 0);
    }

    #[test]
    fn a_word_standing_alone_beats_the_start_of_a_longer_name() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("nibble.go", root, false);
        builder.push("NIB.pdf", root, false);
        let index = builder.finish();
        assert_eq!(first(&index, "nib"), "/home/NIB.pdf");
    }

    #[test]
    fn the_shorter_of_two_equal_names_wins() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("plan.html", root, false);
        builder.push("plan.pdf", root, false);
        let index = builder.finish();
        assert_eq!(first(&index, "plan"), "/home/plan.pdf");
    }

    #[test]
    fn a_word_in_a_folder_above_counts_as_a_clean_match() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("sync_energy_ready_made.txt", root, false);
        let synergy = builder.push("synergy-core", root, true);
        builder.push("README.md", synergy, false);
        let index = builder.finish();
        assert_eq!(
            first(&index, "synergy readme"),
            "/home/synergy-core/README.md"
        );
    }

    #[test]
    fn the_shallower_of_two_equal_matches_wins() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let deep = builder.push("a", root, true);
        let deeper = builder.push("b", deep, true);
        builder.push("plan.md", deeper, false);
        builder.push("plan.md", deep, false);
        let index = builder.finish();
        assert_eq!(first(&index, "plan"), "/home/a/plan.md");
    }

    #[test]
    fn the_word_plus_an_extension_beats_a_longer_name_above_it() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let ui = builder.push("ui", root, true);
        builder.push("button.tsx", ui, false);
        builder.push("button-submit.tsx", root, false);
        builder.push("button.test.tsx", root, false);
        let index = builder.finish();
        assert_eq!(first(&index, "button"), "/home/ui/button.tsx");
    }

    #[test]
    fn upper_case_names_and_dotfiles_count_as_the_whole_name() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let deep = builder.push("a", root, true);
        builder.push("NIB.pdf", deep, false);
        builder.push(".zshrc", deep, false);
        builder.push("nib-old.pdf", root, false);
        builder.push("zshrc-notes.md", root, false);
        let index = builder.finish();
        assert_eq!(first(&index, "nib"), "/home/a/NIB.pdf");
        assert_eq!(first(&index, "zshrc"), "/home/a/.zshrc");
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
    fn allows_one_typo_for_a_word_no_name_holds() {
        let index = sample();
        assert_eq!(paths(&index, "plna"), ["/home/other/plan.md"]);
        assert_eq!(paths(&index, "other plna"), ["/home/other/plan.md"]);
        assert!(paths(&index, "zzzz").is_empty());
        assert!(paths(&index, "^plna").is_empty());
    }

    #[test]
    fn a_loose_match_does_not_block_a_typo() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("design.pdf", root, false);
        builder.push("d-e-s-i-n-g.txt", root, false);
        let index = builder.finish();
        assert_eq!(
            paths(&index, "desing"),
            ["/home/d-e-s-i-n-g.txt", "/home/design.pdf"]
        );
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
