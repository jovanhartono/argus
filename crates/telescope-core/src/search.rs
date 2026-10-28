//! Search: the letter check, fuzzy matching per word, and dropping searches nobody wants any more.

use crate::apps::{AppId, Apps};
use crate::index::{EntryId, Index, NO_PARENT, NameId, letter, letters};
use crate::within::Within;
use frizbee::{Config, Match, Matcher, Matching, Pattern, Scoring, SortStrategy};
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
const FOLDER_WORD_PENALTY: u32 = 4;
const RARER_BY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub item: Item,
    pub score: u32,
    name_len: u32,
}

/// Apps come first so an app wins a full tie with a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Item {
    App(AppId),
    Entry(EntryId),
}

impl Hit {
    /// The index entry found, unless this is an app.
    pub fn entry(&self) -> Option<EntryId> {
        match self.item {
            Item::Entry(entry) => Some(entry),
            Item::App(_) => None,
        }
    }
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
    app_words: Vec<u8>,
    app_held: Vec<u8>,
    app_score: Vec<u16>,
    /// Tests turn the short-word shortcuts off to check they never change what a search finds.
    #[cfg(test)]
    no_shortcuts: bool,
    #[cfg(test)]
    early_stops: usize,
    #[cfg(test)]
    outside_checks: usize,
    #[cfg(test)]
    later_passes: usize,
}

impl Scratch {
    #[cfg(test)]
    fn shortcuts(&self) -> bool {
        !self.no_shortcuts
    }

    #[cfg(not(test))]
    #[expect(clippy::unused_self, reason = "tests can turn the shortcuts off")]
    fn shortcuts(&self) -> bool {
        true
    }
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

    /// The best `limit` hits among the files and the apps, or `None` when a newer ticket
    /// arrived first.
    pub fn search(
        &self,
        index: &Index,
        apps: &Apps,
        query: &str,
        ticket: Ticket,
        limit: usize,
    ) -> Option<Vec<Hit>> {
        self.search_in(index, apps, None, query, ticket, limit)
    }

    /// As `search`, keeping only the files inside `within`, which was made from this `index`.
    /// They rank as they would in a search of everything, so the folder only filters.
    pub fn search_in(
        &self,
        index: &Index,
        apps: &Apps,
        within: Option<&Within>,
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
            let hits = run(index, apps, query, limit, within, scratch, &stale)?;
            (!stale()).then_some(hits)
        })
    }
}

fn run(
    index: &Index,
    apps: &Apps,
    query: &str,
    limit: usize,
    within: Option<&Within>,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<Vec<Hit>> {
    let names = within.map(|within| within.names.as_slice());
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
    let limit = limit.min(index.len() + apps.len());
    if wanted == 0 || limit == 0 {
        return Some(Vec::new());
    }

    let mut app_hits = Vec::new();
    let (app_matched, app_held) =
        match_apps(apps, &words, wanted, unwanted, 0, scratch, &mut app_hits);
    let later = common_words(index, &words, wanted, typo_ready);
    // "38" fits a third of all names, mostly hashes that only scatter its letters. Names where
    // it starts the name or a word go first, and the rest only when those cannot fill the list.
    let ceiling = match words.as_slice() {
        [word] if scratch.shortcuts() => plain_ceiling(word, &matcher_config().scoring),
        _ => None,
    };
    let (mut matched, held) = match_names(
        index,
        &words,
        names,
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
    // name holds is probably misspelled, like "desing", so it may take one typo. An app's
    // name counts, so "safari" finds Safari, not files a typo away from it.
    let mut unsure = wanted & typo_ready & !(held | app_held);
    if unsure != 0 && names.is_some() {
        // "desing" in a folder of design.pdf finds nothing when desing.txt sits elsewhere.
        #[cfg(test)]
        {
            scratch.outside_checks += 1;
        }
        let pass = Pass::Outside;
        unsure &= !match_names(index, &words, names, unsure, unsure, pass, scratch, stale)?.1;
    }
    if within.is_some() {
        // Apps live outside the user's folders, so a search inside one shows none.
        app_hits.clear();
    } else if unsure != 0 && wanted & !app_matched & !unsure == 0 {
        app_hits.clear();
        match_apps(
            apps,
            &words,
            wanted,
            unwanted,
            unsure,
            scratch,
            &mut app_hits,
        );
    }
    if unsure != 0 && wanted & !later & !matched & !unsure == 0 {
        (matched, _) = match_names(
            index,
            &words,
            names,
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
        mark_possible(index, within, wanted & !later, unwanted, scratch, stale)?;
        #[cfg(test)]
        {
            scratch.later_passes += 1;
        }
        let pass = Pass::Possible;
        matched |= match_names(index, &words, names, later, wanted, pass, scratch, stale)?.0;
    }
    // Room for the bundles an app hit may hide, so `limit` files are left once they are
    // dropped. Only apps inside the indexed folder have one, like those in ~/Applications.
    keep_best(&mut app_hits, limit);
    let inside = |hit: &&Hit| match hit.item {
        Item::App(app) => index.len() > 0 && apps.folder(app).starts_with(index.name(0)),
        Item::Entry(_) => false,
    };
    let wide = limit + app_hits.iter().filter(inside).count();
    // A word found in a folder above counts almost as a clean match of that word, so
    // "willow readme" ranks willow-core/README.md above a long name that scatters the letters
    // of both words, while for "shop workspace" Shop.xcworkspace still beats
    // shop-web/.idea/workspace.xml.
    let match_score = u32::from(Config::default().scoring.match_score);
    let mut credit = [0u32; MAX_WORDS];
    for (credit, word) in credit.iter_mut().zip(&words) {
        *credit =
            (match_score * word.needle.chars().count() as u32).saturating_sub(FOLDER_WORD_PENALTY);
    }
    let mut folder_bonus = [0u32; 1 << MAX_WORDS];
    for mask in 1..folder_bonus.len() {
        folder_bonus[mask] =
            folder_bonus[mask & (mask - 1)] + credit[mask.trailing_zeros() as usize];
    }
    if let Some(ceiling) = ceiling {
        // Most files sit several folders down. With fewer than `limit` names that would beat
        // every plain name five folders deep, "l59" would read every entry twice, so skip ahead.
        let deep = u32::from(ceiling) + 4 * DEPTH_PENALTY;
        let mut winners = scratch.name_score.iter().filter(|&&s| u32::from(s) > deep);
        if winners.nth(wide - 1).is_some() {
            let hits = best(
                index,
                within,
                scratch,
                wanted,
                unwanted,
                &folder_bonus,
                wide,
                stale,
            )?;
            // Every entry sits at least one folder deep, which costs a plain name a little more.
            let plain_best = u32::from(ceiling).saturating_sub(DEPTH_PENALTY);
            if hits.len() == wide && hits[wide - 1].score > plain_best {
                #[cfg(test)]
                {
                    scratch.early_stops += 1;
                }
                return Some(with_apps(index, apps, hits, app_hits, limit));
            }
        }
        let pass = Pass::Rest;
        matched |= match_names(index, &words, names, wanted, wanted, pass, scratch, stale)?.0;
    }
    if wanted & !matched != 0 {
        return Some(with_apps(index, apps, Vec::new(), app_hits, limit));
    }
    if uses_folders(wanted, unwanted) {
        words_above(index, scratch, stale)?;
    }
    let hits = best(
        index,
        within,
        scratch,
        wanted,
        unwanted,
        &folder_bonus,
        wide,
        stale,
    )?;
    Some(with_apps(index, apps, hits, app_hits, limit))
}

/// One word must be in the name itself; more words, or an exclusion, also look at the folders.
fn uses_folders(wanted: u8, unwanted: u8) -> bool {
    wanted.count_ones() > 1 || unwanted != 0
}

/// The best `limit` entries whose path holds every `wanted` word and no `unwanted` one.
#[expect(
    clippy::too_many_arguments,
    reason = "one search's state, split for the borrow checker"
)]
fn best(
    index: &Index,
    within: Option<&Within>,
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
    let entries = within.map_or(0..index.len(), |within| within.entries.clone());
    let start = entries.start;
    let chunks: Vec<Vec<Hit>> = index.entry_name[entries.clone()]
        .par_chunks(ENTRY_CHUNK)
        .zip(index.entry_parent[entries.clone()].par_chunks(ENTRY_CHUNK))
        .zip(index.entry_depth[entries].par_chunks(ENTRY_CHUNK))
        .enumerate()
        .map(|(chunk, ((names, parents), depths))| {
            if stale() {
                return None;
            }
            let first = start + chunk * ENTRY_CHUNK;
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
                if within.is_some_and(|within| !within.holds(parent)) {
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
                    item: Item::Entry((first + i) as EntryId),
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

/// The best `limit` of the files and apps found. A bundle the index holds too, like Gmail.app
/// in ~/Applications, shows once, as the app, and only when the app itself was found.
fn with_apps(
    index: &Index,
    apps: &Apps,
    mut hits: Vec<Hit>,
    app_hits: Vec<Hit>,
    limit: usize,
) -> Vec<Hit> {
    if !app_hits.is_empty() {
        hits.retain(|hit| !shown_as_app(index, apps, hit, &app_hits));
        hits.extend(app_hits);
    }
    keep_best(&mut hits, limit);
    hits
}

fn shown_as_app(index: &Index, apps: &Apps, hit: &Hit, app_hits: &[Hit]) -> bool {
    let Item::Entry(entry) = hit.item else {
        return false;
    };
    let Some(name) = index.name(entry).strip_suffix(".app") else {
        return false;
    };
    // Paths are built only for a name an app hit shares, a few at most per search.
    app_hits.iter().any(|app_hit| match app_hit.item {
        Item::App(app) => apps.name(app) == name && apps.path(app) == index.path(entry),
        Item::Entry(_) => false,
    })
}

/// Apps are matched on their names alone: each wanted word in the name and no unwanted one.
/// With no folders above them, they lose nothing for depth, so an app ranks above a file whose
/// name matches as well. Returns the words that matched at least one app, and the words the
/// apps found hold letter for letter.
fn match_apps(
    apps: &Apps,
    words: &[Pattern],
    wanted: u8,
    unwanted: u8,
    typos: u8,
    scratch: &mut Scratch,
    hits: &mut Vec<Hit>,
) -> (u8, u8) {
    if apps.is_empty() {
        return (0, 0);
    }
    let config = matcher_config();
    let Scratch {
        app_words,
        app_held,
        app_score,
        ..
    } = scratch;
    for list in [&mut *app_words, &mut *app_held] {
        list.clear();
        list.resize(apps.len(), 0);
    }
    app_score.clear();
    app_score.resize(apps.len(), 0);
    let mut matched = 0u8;
    for (w, word) in words.iter().enumerate() {
        let word = word.clone().max_typos(Some(u16::from(typos >> w & 1)));
        for found in Matcher::new(word, &config).match_list(&apps.names) {
            let n = found.index as usize;
            app_words[n] |= 1 << w;
            matched |= 1 << w;
            if wanted >> w & 1 == 1 {
                let name = &apps.names[n];
                let (score, holds) = points(found, name, &words[w].needle, 0, &config.scoring);
                app_held[n] |= u8::from(holds) << w;
                app_score[n] = app_score[n].saturating_add(score);
            }
        }
    }
    // Photos holding "photos" says nothing about the spelling of "photos trip" when Photos
    // lacks "trip", so only the apps found count.
    let mut held = 0u8;
    for (n, &own) in app_words.iter().enumerate() {
        if own & wanted == wanted && own & unwanted == 0 {
            held |= app_held[n];
            hits.push(Hit {
                item: Item::App(n as AppId),
                score: u32::from(app_score[n]),
                name_len: apps.names[n].len() as u32,
            });
        }
    }
    (matched, held)
}

fn matcher_config() -> Config {
    let mut config = Config::default().sort(SortStrategy::IndexAsc);
    // Typing in lower case must not favour lower-case names over NIB.pdf.
    config.scoring.matching_case_bonus = 0;
    config
}

/// A name's points for one word, and whether it holds the word letter for letter. Below
/// `floor` it cannot, so the name is not read again.
fn points(found: Match, name: &str, word: &str, floor: u16, scoring: &Scoring) -> (u16, bool) {
    let score = found.score;
    let how = if score < floor {
        Held::No
    } else {
        held_in(name, word)
    };
    match how {
        how @ (Held::Whole | Held::Alone) => {
            let exact = if matches!(how, Held::Whole) && !found.exact {
                scoring.exact_match_bonus
            } else {
                0
            };
            (
                score.saturating_add(exact).saturating_add(WHOLE_WORD_BONUS),
                true,
            )
        }
        Held::Inside => (score, true),
        Held::No => (score, false),
    }
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
    /// Only the names left out of `names`, to learn which words a name there holds.
    Outside,
}

/// Each distinct name is matched once, however many folders hold it, for the `active` words;
/// only those set in `names` when given, or for `Pass::Outside` those not set. Returns the words that matched at least one name, and
/// the search words some name holds.
#[expect(
    clippy::too_many_arguments,
    reason = "one search's state, split for the borrow checker"
)]
fn match_names(
    index: &Index,
    words: &[Pattern],
    names: Option<&[u64]>,
    active: u8,
    wanted: u8,
    pass: Pass,
    scratch: &mut Scratch,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<(u8, u8)> {
    let config = matcher_config();
    let typo_words = match pass {
        Pass::Everywhere { typos } => typos,
        Pass::Strong | Pass::Rest | Pass::Possible | Pass::Outside => 0,
    };
    let typos = |w: usize| u16::from(typo_words >> w & 1);
    let needed: Vec<u64> = words.iter().map(|word| letters(&word.needle)).collect();
    let shortcuts = scratch.shortcuts();
    let floors: Vec<u16> = (0..words.len())
        .map(|w| held_floor(&words[w], typos(w), &config.scoring))
        .map(|floor| if shortcuts { floor } else { 0 })
        .collect();
    let one_letters: Vec<Option<OneLetter>> = (0..words.len())
        .map(|w| OneLetter::new(&words[w], typos(w)).filter(|_| shortcuts))
        .collect();
    let Scratch {
        name_words,
        name_score,
        possible,
        ..
    } = scratch;
    let only_possible = matches!(pass, Pass::Possible);
    let outside = matches!(pass, Pass::Outside);
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
                        if names
                            .is_some_and(|names| (names[n / 64] >> (n % 64) & 1 == 1) == outside)
                        {
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
                            Pass::Everywhere { .. } | Pass::Possible | Pass::Outside => false,
                        };
                        if skip {
                            continue;
                        }
                        if let Some(one) = one_letters[w]
                            && have != u64::MAX
                        {
                            bits[n - first] |= 1 << w;
                            matched |= 1 << w;
                            if wanted >> w & 1 == 1 {
                                let text = || index.name_text(n as NameId);
                                let score = one.score(starts(), text, &config.scoring);
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
                            let text = texts[found.index as usize];
                            let (score, holds) =
                                points(found, text, &words[w].needle, floors[w], &config.scoring);
                            held |= u8::from(holds) << w;
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
    /// needs the name read; starting the name earns the most. A capital after a lower-case
    /// letter earns `capitalization_bonus`, which is as much as `delimiter_bonus`, so the name
    /// is read no further than its first letter.
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
    within: Option<&Within>,
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
    let entries = within.map_or(0..index.len(), |within| within.entries.clone());
    index.entry_name[entries.clone()]
        .par_chunks(ENTRY_CHUNK)
        .zip(index.entry_parent[entries].par_chunks(ENTRY_CHUNK))
        .try_for_each(|(names, parents)| {
            if stale() {
                return None;
            }
            for (&name, &parent) in names.iter().zip(parents) {
                if parent == NO_PARENT || within.is_some_and(|within| !within.holds(parent)) {
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
        .then(a.item.cmp(&b.item))
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
    use std::path::Path;

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
        let willow = builder.push("willow-core", root, true);
        let other = builder.push("other", root, true);
        builder.push("README.md", willow, false);
        builder.push("README.md", other, false);
        builder.push("plan.md", other, false);
        builder.finish()
    }

    /// Best first, apps by their bundle's path.
    fn ranked(index: &Index, apps: &Apps, query: &str) -> Vec<String> {
        let searcher = Searcher::with_threads(2);
        let hits = searcher
            .search(index, apps, query, searcher.ticket(), 10)
            .unwrap();
        hits.iter()
            .map(|hit| match hit.item {
                Item::Entry(entry) => index.path(entry),
                Item::App(app) => apps.path(app),
            })
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    fn paths(index: &Index, query: &str) -> Vec<String> {
        let mut found = ranked(index, &Apps::default(), query);
        found.sort();
        found
    }

    fn first(index: &Index, query: &str) -> String {
        ranked(index, &Apps::default(), query).swap_remove(0)
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

        /// Mostly short names, with one in eight long enough to cross frizbee's 16, 32 and
        /// 64-byte blocks the way hashes and long file names do.
        fn name(&mut self, alphabet: &[&str], short: usize) -> String {
            let max = if self.below(8) == 0 { 250 } else { short };
            self.text(alphabet, max)
        }
    }

    #[test]
    fn held_floor_never_hides_a_held_word() {
        let mut rng = Rng(0x5eed);
        let names = [
            "a", "b", "3", "8", "A", "B", ".", "-", "_", " ", "/", "(", "é", "a", "3",
        ];
        let lower = ["a", "b", "3", "8", ".", "_"];
        let config = matcher_config();
        let mut checked = 0;
        for _ in 0..3_000 {
            let needle = match rng.below(4) {
                0 => rng.text(&lower, 1).to_uppercase() + &rng.text(&lower, 3),
                _ => rng.text(&lower, 4),
            };
            let typed = match rng.below(4) {
                0 => format!("^{needle}"),
                1 => format!("{needle}$"),
                2 => format!("'{needle}"),
                _ => needle.clone(),
            };
            let word = Pattern::parse_query(&typed).remove(0).max_typos(Some(0));
            assert_eq!(word.needle, needle);
            let floor = held_floor(&word, 0, &config.scoring);
            let haystacks: Vec<String> = (0..40).map(|_| rng.name(&names, 30)).collect();
            for found in Matcher::new(word.clone(), &config).match_list(&haystacks) {
                let name = &haystacks[found.index as usize];
                if floor > 0 && !matches!(held_in(name, &needle), Held::No) {
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
        let config = matcher_config();
        let mut checked = 0;
        for typed in ["a", "b", "c", "3", "z"] {
            let word = positive(Pattern::parse_query(typed).remove(0)).max_typos(Some(0));
            let one = OneLetter::new(&word, 0).unwrap();
            let haystacks: Vec<String> = (0..20_000).map(|_| rng.name(&names, 12)).collect();
            let stripped: Vec<&str> = haystacks
                .iter()
                .map(|name| name.strip_prefix('.').unwrap_or(name))
                .collect();
            let mut frizbee = vec![None; haystacks.len()];
            for found in Matcher::new(word.clone(), &config).match_list(&stripped) {
                frizbee[found.index as usize] = Some(found.score);
            }
            for (name, expected) in haystacks.iter().zip(frizbee) {
                let ours = (letters(name) & one.bit != 0)
                    .then(|| one.score(word_starts(name), || name, &config.scoring));
                assert_eq!(ours, expected, "{typed:?} in {name:?}");
                checked += usize::from(ours.is_some());
            }
        }
        assert!(checked > 20_000);
    }

    /// Frizbee's bonuses depend only on the byte before, so every pair of bytes, at the start,
    /// after a dot and inside a name, covers every case.
    #[test]
    fn one_letter_scores_and_word_starts_match_frizbee_for_every_pair() {
        let config = matcher_config();
        let chars: Vec<char> = (' '..='~').chain(['é', '日']).collect();
        let mut haystacks = Vec::new();
        for &a in &chars {
            for &b in &chars {
                haystacks.extend([format!("{a}{b}"), format!(".{a}{b}"), format!("q{a}{b}")]);
            }
        }
        let stripped: Vec<&str> = haystacks
            .iter()
            .map(|name| name.strip_prefix('.').unwrap_or(name))
            .collect();
        for typed in ('a'..='z').chain('0'..='9') {
            let word = positive(Pattern::parse_query(&typed.to_string()).remove(0));
            let one = OneLetter::new(&word, 0).unwrap();
            let ceiling = plain_ceiling(&word, &config.scoring).unwrap();
            let mut frizbee = vec![None; haystacks.len()];
            for found in Matcher::new(word.clone(), &config).match_list(&stripped) {
                frizbee[found.index as usize] = Some(found.score);
            }
            for (name, expected) in haystacks.iter().zip(frizbee) {
                let starts = word_starts(name);
                if starts & one.bit == 0 {
                    assert!(expected <= Some(ceiling), "{typed:?} in {name:?}");
                }
                if name.is_ascii() {
                    let ours = (letters(name) & one.bit != 0)
                        .then(|| one.score(starts, || name, &config.scoring));
                    assert_eq!(ours, expected, "{typed:?} in {name:?}");
                }
            }
        }
    }

    #[test]
    fn a_plain_name_never_scores_above_the_ceiling() {
        let mut rng = Rng(0xfeed);
        let names = [
            "a", "b", "3", "8", "A", "B", ".", "-", "_", " ", "(", "é", "a", "3", "8",
        ];
        let lower = ["a", "b", "3", "8"];
        let config = matcher_config();
        let mut checked = 0;
        for _ in 0..2_000 {
            let typed = rng.text(&lower, 3);
            // A quoted word goes through frizbee's literal scorer instead.
            let query = if rng.below(2) == 0 {
                format!("'{typed}")
            } else {
                typed.clone()
            };
            let word = positive(Pattern::parse_query(&query).remove(0)).max_typos(Some(0));
            let ceiling = plain_ceiling(&word, &config.scoring).unwrap();
            let needed = letters(&typed);
            let haystacks: Vec<String> = (0..40).map(|_| rng.name(&names, 16)).collect();
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
        let scoring = matcher_config().scoring;
        let word = |text: &str| positive(Pattern::parse_query(text).remove(0));
        assert_eq!(plain_ceiling(&word("c"), &scoring), Some(12));
        assert_eq!(plain_ceiling(&word("7"), &scoring), Some(12));
        assert_eq!(plain_ceiling(&word("385"), &scoring), Some(36));
        assert_eq!(plain_ceiling(&word("'cv"), &scoring), Some(24));
        assert_eq!(plain_ceiling(&word("3857"), &scoring), None);
        assert_eq!(plain_ceiling(&word("Cv"), &scoring), None);
        assert_eq!(plain_ceiling(&word("a.b"), &scoring), None);
        assert_eq!(plain_ceiling(&word("^cv"), &scoring), None);
        assert_eq!(plain_ceiling(&word("cv$"), &scoring), None);
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
        let names: Vec<String> = (0..30)
            .map(|i| format!("{}{i}", rng.text(&parts, 5)))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let searcher = Searcher::with_threads(2);
        for apps in [Apps::default(), applications(&names)] {
            for query in ["3", "38", "x8", "38x", "y", "38 x", "x 3", "3 !yy"] {
                let all = searcher
                    .search(&index, &apps, query, searcher.ticket(), 1_000)
                    .unwrap();
                assert!(all.len() > 20, "{query:?}");
                for limit in 1..=all.len() {
                    let few = searcher
                        .search(&index, &apps, query, searcher.ticket(), limit)
                        .unwrap();
                    assert_eq!(few, all[..limit], "{query:?} limit {limit}");
                }
            }
        }
    }

    /// The search with and without the short-word shortcuts, which must agree.
    fn with_and_without_shortcuts(
        index: &Index,
        apps: &Apps,
        query: &str,
        limit: usize,
    ) -> (Vec<Hit>, usize) {
        let mut fast = Scratch::default();
        let mut slow = Scratch {
            no_shortcuts: true,
            ..Scratch::default()
        };
        let hits = run(index, apps, query, limit, None, &mut fast, &|| false).unwrap();
        let expected = run(index, apps, query, limit, None, &mut slow, &|| false).unwrap();
        assert_eq!(hits, expected, "{query:?} limit {limit}");
        (hits, fast.early_stops)
    }

    #[test]
    fn the_shortcuts_never_change_what_a_search_finds() {
        let mut rng = Rng(0x5107);
        let rich = [
            "3", "8", "c", "a", "b", "x", "C", "A", "-", ".", " ", "é", "_", "3", "c",
        ];
        // Only lower case and digits, so a word mostly sits inside a name, not at a word start.
        let flat = ["3", "8", "c", "a", "b", "x", "q"];
        let mut early_stops = 0;
        for round in 0..24 {
            let parts: &[&str] = if round % 2 == 0 { &rich } else { &flat };
            let mut builder = IndexBuilder::new();
            let root = builder.push("/home", NO_PARENT, true);
            let mut folders = vec![root];
            for i in 0..300 {
                let parent = folders[rng.below(folders.len())];
                let mut name = rng.name(parts, 10);
                if rng.below(10) == 0 {
                    name.insert(0, '.');
                }
                let entry = builder.push(&name, parent, i % 6 == 0);
                if i % 6 == 0 {
                    folders.push(entry);
                }
            }
            // Every other round, apps, some of them also bundles in the index.
            let names: Vec<String> = (0..6 * (round % 2)).map(|_| rng.name(parts, 6)).collect();
            let applications = builder.push("Applications", root, true);
            for name in &names[..names.len() / 2] {
                builder.push(&format!("{name}.app"), applications, true);
            }
            let index = builder.finish();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let apps = Apps::from_names(Path::new("/home/Applications"), &names);
            for query in [
                "c", "3", "38", "x8", "38x", "'c", "'38", "^c", "c$", "C", ".c", "c a", "38 c",
                "a !b", "cab", "cab 38x",
            ] {
                for limit in [1, 3, 10, 40, 1_000] {
                    early_stops += with_and_without_shortcuts(&index, &apps, query, limit).1;
                }
            }
        }
        assert!(early_stops > 100, "{early_stops}");
    }

    fn is_inside(index: &Index, entry: EntryId, folder: EntryId) -> bool {
        let mut at = index.entry_parent[entry as usize];
        while at != NO_PARENT {
            if at == folder {
                return true;
            }
            at = index.entry_parent[at as usize];
        }
        false
    }

    /// The search inside `folder`, against a search of everything with the apps and what lies
    /// outside dropped, for each limit; with and without the short-word shortcuts. Returns how
    /// many hits lie inside, and the scratch of the searches with shortcuts.
    fn assert_scoped_is_filtered(
        index: &Index,
        apps: &Apps,
        folder: EntryId,
        query: &str,
    ) -> (usize, Scratch) {
        let within = Within::new(index, folder);
        let all = run(
            index,
            apps,
            query,
            index.len() + apps.len(),
            None,
            &mut Scratch::default(),
            &|| false,
        );
        let kept: Vec<Hit> = all
            .unwrap()
            .into_iter()
            .filter(|hit| {
                hit.entry()
                    .is_some_and(|entry| is_inside(index, entry, folder))
            })
            .collect();
        let mut scratch = Scratch::default();
        let mut slow = Scratch {
            no_shortcuts: true,
            ..Scratch::default()
        };
        let within = Some(&within);
        for limit in [1, 3, 10, kept.len() + 1] {
            let expected = &kept[..limit.min(kept.len())];
            let found = run(index, apps, query, limit, within, &mut scratch, &|| false).unwrap();
            assert_eq!(found, expected, "{query:?} in {folder} limit {limit}");
            let found = run(index, apps, query, limit, within, &mut slow, &|| false).unwrap();
            assert_eq!(
                found, expected,
                "{query:?} in {folder} limit {limit}, no shortcuts"
            );
        }
        (kept.len(), scratch)
    }

    #[test]
    fn a_search_inside_a_folder_finds_what_a_search_of_everything_finds_there() {
        let mut rng = Rng(0x7ab);
        let rich = [
            "3", "8", "c", "a", "b", "x", "C", "A", "-", ".", " ", "é", "_", "3", "c",
        ];
        // A name with an accent counts as holding every letter, so only these let "c" fit far
        // more names than "zq".
        let ascii = &rich[..11];
        let (mut found, mut early_stops, mut later, mut outside_checks) = (0, 0, 0, 0);
        let mut check = |index: &Index, apps: &Apps, folder: EntryId, query: &str| {
            let (kept, scratch) = assert_scoped_is_filtered(index, apps, folder, query);
            found += kept;
            early_stops += scratch.early_stops;
            later += scratch.later_passes;
            outside_checks += scratch.outside_checks;
        };
        for round in 0..12 {
            let mut builder = IndexBuilder::new();
            let root = builder.push("/home", NO_PARENT, true);
            let mut folders = vec![root];
            // Folders and files in random order, so a folder's children sit apart, as in
            // made-up names and after an update.
            let parts: &[&str] = if round % 2 == 0 { &rich } else { ascii };
            for i in 0..400 {
                let parent = folders[rng.below(folders.len())];
                let mut name = rng.name(parts, 8);
                if rng.below(10) == 0 {
                    name.insert(0, '.');
                }
                // A word few names hold, so "zq c" matches "c" last.
                if rng.below(8) == 0 {
                    name.push_str(" zq");
                }
                let is_dir = rng.below(4) == 0 || i < 8 * round;
                let entry = builder.push(&name, parent, is_dir);
                if is_dir {
                    folders.push(entry);
                }
            }
            let index = builder.finish();
            // Every other round, apps, whose names also decide whether a word may take a typo.
            let names: Vec<String> = (0..20 * (round / 2 % 2))
                .map(|_| rng.name(parts, 8))
                .collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let apps = applications(&names);
            let queries = [
                "c", "3", "38", "x8", "38x", "'c", "^c", "c$", "C", ".c", "c a", "38 c", "a !b",
                "cab", "c3a8x", "a b c", "!c", "zq c", "zq 38 !b",
            ];
            for _ in 0..6 {
                let folder = folders[rng.below(folders.len())];
                for query in queries {
                    check(&index, &apps, folder, query);
                }
                // Words cut from real names, some with two letters swapped, so a word is held
                // letter for letter in one folder and needs a typo in another.
                for _ in 0..6 {
                    let name: Vec<char> = index
                        .name(rng.below(index.len()) as EntryId)
                        .chars()
                        .collect();
                    if name.len() < 5 {
                        continue;
                    }
                    let start = rng.below(name.len() - 4);
                    let mut word: Vec<char> = name[start..start + 4 + rng.below(2)].to_vec();
                    if rng.below(2) == 0 {
                        word.swap(1, 2);
                    }
                    let word: String = word.into_iter().filter(|c| *c != ' ').collect();
                    check(&index, &apps, folder, &word);
                    check(&index, &apps, folder, &format!("c {word}"));
                }
            }
            for query in queries {
                check(&index, &apps, root, query);
            }
        }
        assert!(found > 10_000, "{found}");
        // Each shortcut was taken inside a folder, not just allowed.
        assert!(early_stops > 100, "{early_stops}");
        assert!(later > 100, "{later}");
        assert!(outside_checks > 100, "{outside_checks}");
    }

    /// Whether "desing" may take a typo is decided by every name, so a folder holding only
    /// design.pdf finds what a search of everything shows there: nothing, as desing.txt elsewhere
    /// holds the word letter for letter.
    #[test]
    fn a_typo_inside_a_folder_is_decided_by_every_name() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let work = builder.push("work", root, true);
        builder.push("design.pdf", work, false);
        builder.push("designer notes.md", work, false);
        let other = builder.push("other", root, true);
        builder.push("desing.txt", other, false);
        let index = builder.finish();
        let found_with = |apps: &Apps, folder: EntryId, query: &str| {
            let (kept, _) = assert_scoped_is_filtered(&index, apps, folder, query);
            let mut scratch = Scratch::default();
            let within = Some(Within::new(&index, folder));
            run(
                &index,
                apps,
                query,
                10,
                within.as_ref(),
                &mut scratch,
                &|| false,
            );
            (kept, scratch.outside_checks)
        };
        let found = |folder: EntryId, query: &str| found_with(&Apps::default(), folder, query);
        // An app's name counts as well, as it does in a search of everything.
        let apps = applications(&["Dseign"]);
        assert_eq!(found_with(&apps, work, "dseign"), (0, 0));
        assert_eq!(found_with(&apps, work, "desing"), (0, 1));
        // Only a word no name inside holds makes the search look at the names outside.
        assert_eq!(found(work, "desing"), (0, 1));
        assert_eq!(found(other, "desing"), (1, 0));
        assert_eq!(found(work, "design"), (2, 0));
        assert_eq!(found(work, "work desing"), (0, 1));
        // No name anywhere holds these, so they take a typo inside the folder too.
        assert_eq!(found(work, "dseign"), (2, 1));
        assert_eq!(found(work, "notes desgn"), (1, 1));
        assert_eq!(found(work, "notes desgner"), (1, 1));
        assert_eq!(found(work, "!desing notes"), (1, 0));
    }

    #[test]
    fn a_search_inside_a_folder_skips_the_folder_itself_and_its_neighbours() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let plans = builder.push("plans", root, true);
        let file = builder.push("plan.md", root, false);
        let old = builder.push("old", plans, true);
        builder.push("plan.md", old, false);
        let other = builder.push("other plans", root, true);
        builder.push("plan.md", other, false);
        builder.push("plan.pdf", plans, false);
        let index = builder.finish();
        let searcher = Searcher::with_threads(2);
        let apps = applications(&["Plan"]);
        let paths = |folder: EntryId, query: &str| -> Vec<String> {
            let within = Some(Within::new(&index, folder));
            let ticket = searcher.ticket();
            let hits = searcher.search_in(&index, &apps, within.as_ref(), query, ticket, 10);
            hits.unwrap()
                .iter()
                .map(|hit| hit.entry().expect("no app inside a folder"))
                .map(|entry| index.path(entry).to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(
            paths(plans, "plan"),
            ["/home/plans/plan.pdf", "/home/plans/old/plan.md"]
        );
        assert_eq!(paths(old, "plan"), ["/home/plans/old/plan.md"]);
        assert_eq!(paths(old, "old"), Vec::<String>::new());
        // The folder's own name counts for what is inside it, as in a search of everything.
        assert_eq!(paths(plans, "plans old"), ["/home/plans/old"]);
        assert!(paths(plans, "").is_empty());
        assert!(paths(file, "plan").is_empty());
    }

    #[test]
    fn a_short_word_inside_names_is_still_found() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("a38b.txt", root, false);
        builder.push("x3y8", root, false);
        builder.push("bcb", root, false);
        let index = builder.finish();
        assert_eq!(paths(&index, "38"), ["/home/a38b.txt", "/home/x3y8"]);
        assert_eq!(paths(&index, "c"), ["/home/bcb"]);
    }

    #[test]
    fn one_letter_never_matches_a_name_with_accents_that_lacks_it() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("café.txt", root, false);
        builder.push("notes.md", root, false);
        let index = builder.finish();
        assert!(paths(&index, "z").is_empty());
        assert_eq!(paths(&index, "c"), ["/home/café.txt"]);
        let (hits, _) = with_and_without_shortcuts(&index, &Apps::default(), "c", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_word_scoring_exactly_the_floor_still_counts_as_held() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("é38.pdf", root, false);
        builder.push("x38.pdf", root, false);
        builder.push("xdesingx.txt", root, false);
        builder.push("design.pdf", root, false);
        let index = builder.finish();
        // Frizbee gives "38" no bonus after "é", yet it stands alone there.
        assert_eq!(first(&index, "38"), "/home/é38.pdf");
        // "desing" held letter for letter blocks the typo that would find design.pdf.
        assert_eq!(paths(&index, "desing"), ["/home/xdesingx.txt"]);
    }

    #[test]
    fn a_short_search_gives_up_partway_when_replaced() {
        let index = sample();
        let checks = AtomicU64::new(0);
        let stale = || checks.fetch_add(1, Ordering::Relaxed) > 0;
        let apps = applications(&["Reminders"]);
        let scratch = &mut Scratch::default();
        assert_eq!(run(&index, &apps, "re", 10, None, scratch, &stale), None);
        assert_eq!(checks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn one_letter_relies_on_equal_word_start_bonuses() {
        let scoring = matcher_config().scoring;
        assert_eq!(scoring.capitalization_bonus, scoring.delimiter_bonus);
        assert!(scoring.prefix_bonus >= scoring.delimiter_bonus);
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
        let scoring = matcher_config().scoring;
        let word = |text: &str| Pattern::parse_query(text).remove(0);
        assert_eq!(held_floor(&word("38"), 0, &scoring), 24);
        assert_eq!(held_floor(&word("a.b"), 0, &scoring), 36);
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
        builder.push("wild_lowly_ready_made.txt", root, false);
        let willow = builder.push("willow-core", root, true);
        builder.push("README.md", willow, false);
        let index = builder.finish();
        assert_eq!(
            first(&index, "willow readme"),
            "/home/willow-core/README.md"
        );
    }

    /// Worth two folder levels: a name one folder deeper still wins, two deeper ties on score.
    #[test]
    fn a_word_in_the_name_beats_the_same_word_in_a_folder_up_to_one_level_deeper() {
        let mut rng = Rng(0x5_0b0);
        // Separate letters, so neither word turns up in the other's name or in the folders.
        let ones = ["f", "g", "j", "k", "n"];
        let twos = ["q", "r", "t", "u", "v", "y", "z"];
        let mut pairs = vec![("shop".to_owned(), "workspace".to_owned())];
        for _ in 0..40 {
            let one = format!("{}{}", rng.text(&ones, 1), rng.text(&ones, 7));
            let two = format!("{}{}", rng.text(&twos, 1), rng.text(&twos, 7));
            pairs.push((one, two));
        }
        for (one, two) in &pairs {
            let query = format!("{one} {two}");
            let mut capital = one.clone();
            capital[..1].make_ascii_uppercase();
            for extra in [false, true] {
                let mut builder = IndexBuilder::new();
                let root = builder.push("/home", NO_PARENT, true);
                let web = builder.push(&format!("{one}-web"), root, true);
                let idea = builder.push(".idea", web, true);
                builder.push(&format!("{two}.xml"), idea, false);
                let app = builder.push(&format!("{one}-app"), root, true);
                let mut ios = builder.push("ios", app, true);
                if extra {
                    ios = builder.push("0", ios, true);
                }
                let name = format!("{capital}.xc{two}");
                builder.push(&name, ios, false);
                let index = builder.finish();
                assert!(
                    first(&index, &query).ends_with(&name),
                    "{query:?} extra {extra}"
                );
            }
        }
    }

    #[test]
    fn a_one_letter_word_in_a_folder_still_beats_the_letter_inside_a_name() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("cv-backup.pdf", root, false);
        let a = builder.push("a", root, true);
        builder.push("cv.pdf", a, false);
        let index = builder.finish();
        assert_eq!(
            ranked(&index, &Apps::default(), "cv a"),
            ["/home/a/cv.pdf", "/home/cv-backup.pdf"]
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
            ["/home/other/README.md", "/home/willow-core/README.md"]
        );
        assert!(paths(&index, "home").is_empty());
    }

    #[test]
    fn other_words_may_match_a_folder_above() {
        let index = sample();
        assert_eq!(
            paths(&index, "willow readme"),
            ["/home/willow-core/README.md"]
        );
        assert_eq!(paths(&index, "readme !willow"), ["/home/other/README.md"]);
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
        let willow = builder.push("willow", root, true);
        builder.push("plan.md", willow, false);
        builder.push("pla.md", root, false);
        let index = builder.finish();
        assert_eq!(paths(&index, "plan"), ["/home/willow/plan.md"]);
        assert!(paths(&index, "plan !willow").is_empty());
    }

    #[test]
    fn a_newer_ticket_cancels_an_older_search() {
        let index = sample();
        let searcher = Searcher::with_threads(2);
        let old = searcher.ticket();
        let _new = searcher.ticket();
        let apps = Apps::default();
        assert_eq!(searcher.search(&index, &apps, "readme", old, 10), None);
    }

    #[test]
    fn a_search_inside_a_folder_gives_up_at_any_check_when_replaced() {
        let index = sample();
        let within = Some(Within::new(&index, 2));
        let within = within.as_ref();
        let apps = applications(&["Reminders"]);
        for query in ["re", "readme", "plna", "other plna", "md !plan"] {
            let checks = AtomicU64::new(0);
            let count = || checks.fetch_add(1, Ordering::Relaxed) == u64::MAX;
            let scratch = &mut Scratch::default();
            assert!(run(&index, &apps, query, 10, within, scratch, &count).is_some());
            let total = checks.load(Ordering::Relaxed);
            assert!(total >= 2, "{query:?} {total}");
            assert_eq!(scratch.outside_checks, usize::from(query.contains("plna")));
            for replaced_at in 0..total {
                let checks = AtomicU64::new(0);
                let stale = || checks.fetch_add(1, Ordering::Relaxed) >= replaced_at;
                let scratch = &mut Scratch::default();
                let hits = run(&index, &apps, query, 10, within, scratch, &stale);
                assert_eq!(hits, None, "{query:?} replaced at check {replaced_at}");
            }
        }
    }

    #[test]
    fn a_search_gives_up_partway_when_replaced() {
        let index = sample();
        let checks = AtomicU64::new(0);
        // Still wanted at the first check, replaced by the second.
        let stale = || checks.fetch_add(1, Ordering::Relaxed) > 0;
        let apps = Apps::default();
        assert_eq!(
            run(
                &index,
                &apps,
                "readme",
                10,
                None,
                &mut Scratch::default(),
                &stale
            ),
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
        let names: Vec<String> = (0..40).map(|i| format!("Plan{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let searcher = Searcher::with_threads(2);
        for apps in [
            Apps::default(),
            Apps::from_names(Path::new("/Applications"), &names),
        ] {
            let all = searcher
                .search(&index, &apps, "plan", searcher.ticket(), 1000)
                .unwrap();
            let few = searcher
                .search(&index, &apps, "plan", searcher.ticket(), 5)
                .unwrap();
            assert_eq!(few, all[..5]);
            assert_eq!(all.len(), 600 + apps.len());
        }
    }

    #[test]
    fn searches_in_a_row_do_not_leak_into_each_other() {
        let index = sample();
        let apps = Apps::from_names(Path::new("/Applications"), &["Willow", "Readme"]);
        let searcher = Searcher::with_threads(2);
        let search = |searcher: &Searcher, query| {
            searcher
                .search(&index, &apps, query, searcher.ticket(), 10)
                .unwrap()
        };
        search(&searcher, "willow readme");
        assert_eq!(search(&searcher, "plan").len(), 1);
        search(&searcher, "willow readme");
        let after = search(&searcher, "willow");
        assert_eq!(after, search(&Searcher::with_threads(2), "willow"));
        assert_eq!(after.len(), 2);
    }

    fn applications(names: &[&str]) -> Apps {
        Apps::from_names(Path::new("/Applications"), names)
    }

    #[test]
    fn an_app_ranks_above_a_file_or_folder_of_the_same_name() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("Notes", root, true);
        builder.push("Notes.md", root, false);
        let deep = builder.push("a", root, true);
        builder.push("notes", deep, false);
        let index = builder.finish();
        let apps = applications(&["Notes", "Notes Helper"]);
        // A capital letter typed must be matched, so "Notes" leaves out a/notes.
        for (query, count) in [("notes", 5), ("Notes", 4), ("note", 5), ("nts", 5)] {
            let found = ranked(&index, &apps, query);
            assert_eq!(found[0], "/Applications/Notes.app", "{query}");
            assert_eq!(found.len(), count, "{query}");
        }
    }

    #[test]
    fn an_app_beats_the_file_with_its_name_for_every_query_that_finds_both() {
        let names = [
            "plan",
            "Budget",
            "Visual Studio Code",
            "Xcode",
            "Photo Booth",
            "café",
            "Go2",
            "my-app",
            "NIB",
            "a",
        ];
        for name in names {
            let mut builder = IndexBuilder::new();
            let root = builder.push("/home", NO_PARENT, true);
            builder.push(name, root, false);
            builder.push(&format!("{name}.txt"), root, false);
            builder.push("unrelated.md", root, false);
            let index = builder.finish();
            let apps = applications(&[name, "Unrelated"]);
            let app = format!("/Applications/{name}.app");
            let file = format!("/home/{name}");
            let letters: Vec<char> = name.chars().collect();
            let mut queries: Vec<String> = vec![name.to_owned(), name.to_lowercase()];
            queries.extend(name.split(' ').map(str::to_owned));
            queries.extend((1..=letters.len()).map(|n| letters[..n].iter().collect()));
            queries.push(format!("{name}!"));
            queries.push(format!("{name} !zzz"));
            queries.push(format!("^{name}"));
            queries.push(format!("{name}$"));
            queries.push(format!("'{name}"));
            // Spread out, like "vsc" for Visual Studio Code.
            let first = name.split(' ').filter_map(|word| word.chars().next());
            queries.push(first.chain(letters.last().copied()).collect());
            if letters.len() >= 4 {
                let mut typo = letters.clone();
                typo.swap(letters.len() - 2, letters.len() - 1);
                queries.push(typo.into_iter().collect());
            }
            for query in &queries {
                let found = ranked(&index, &apps, query);
                let at = |path: &str| found.iter().position(|found| found == path);
                // The same text, matched the same way, wherever it lives.
                assert_eq!(
                    at(&app).is_some(),
                    at(&file).is_some(),
                    "{query:?} {found:?}"
                );
                if let (Some(app), Some(file)) = (at(&app), at(&file)) {
                    assert!(app < file, "{query:?} {found:?}");
                }
            }
        }
    }

    #[test]
    fn an_app_is_found_exactly_when_the_file_of_its_name_is() {
        let mut rng = Rng(0xa995);
        let parts = [
            "a", "b", "e", "s", "z", "A", "B", "S", "0", "7", " ", "-", "é", "ab", "se",
        ];
        let searcher = Searcher::with_threads(2);
        let mut found = 0;
        for _ in 0..400 {
            let name = rng.text(&parts, 12).trim().to_owned();
            if name.is_empty() {
                continue;
            }
            let mut builder = IndexBuilder::new();
            let root = builder.push("/home", NO_PARENT, true);
            builder.push(&name, root, false);
            let index = builder.finish();
            let apps = applications(&[&name]);
            let mut letters: Vec<char> = name.chars().collect();
            let start = rng.below(letters.len());
            letters.drain(..start);
            letters.truncate(rng.below(letters.len()) + 1);
            if letters.len() > 2 && rng.below(3) == 0 {
                letters.remove(rng.below(letters.len()));
            }
            if letters.len() > 3 && rng.below(4) == 0 {
                let at = rng.below(letters.len() - 1);
                letters.swap(at, at + 1);
            }
            let mut query: String = letters.into_iter().collect();
            if rng.below(3) == 0 {
                query = query.to_lowercase();
            }
            let hits = searcher
                .search(&index, &apps, &query, searcher.ticket(), 10)
                .unwrap();
            let file = hits.iter().any(|hit| matches!(hit.item, Item::Entry(_)));
            let app = hits.iter().any(|hit| matches!(hit.item, Item::App(_)));
            assert_eq!(file, app, "{name:?} {query:?}");
            if app {
                found += 1;
                assert!(matches!(hits[0].item, Item::App(_)), "{name:?} {query:?}");
            }
        }
        assert!(found > 200, "{found}");
    }

    #[test]
    fn the_app_holding_every_word_beats_a_file_holding_one_in_its_folder() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let visual = builder.push("visual", root, true);
        builder.push("code.md", visual, false);
        let index = builder.finish();
        let apps = applications(&["Visual Studio Code"]);
        assert_eq!(
            ranked(&index, &apps, "visual code"),
            [
                "/Applications/Visual Studio Code.app",
                "/home/visual/code.md"
            ]
        );
    }

    #[test]
    fn a_word_only_a_failing_app_holds_still_lets_files_take_a_typo() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("Photo trip 2024.zip", root, false);
        let index = builder.finish();
        let apps = applications(&["Photos"]);
        assert_eq!(
            ranked(&index, &apps, "photos trip"),
            ["/home/Photo trip 2024.zip"]
        );
        // Found, Photos does hold the word, so the file is not taken for a typo of it.
        assert_eq!(
            ranked(&index, &apps, "photos"),
            ["/Applications/Photos.app"]
        );
    }

    #[test]
    fn apps_alone_are_trimmed_to_the_limit() {
        let index = IndexBuilder::new().finish();
        let names: Vec<String> = (0..40).map(|i| format!("Safari{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let apps = applications(&names);
        let searcher = Searcher::with_threads(2);
        let all = searcher
            .search(&index, &apps, "safari", searcher.ticket(), 1_000)
            .unwrap();
        let few = searcher
            .search(&index, &apps, "safari", searcher.ticket(), 5)
            .unwrap();
        assert_eq!(all.len(), 40);
        assert_eq!(few, all[..5]);
    }

    #[test]
    fn an_app_typo_pass_starts_over() {
        let index = sample();
        let apps = applications(&["Desk Insight Gallery", "Design"]);
        let mut found = ranked(&index, &apps, "desing");
        found.sort();
        assert_eq!(
            found,
            [
                "/Applications/Design.app",
                "/Applications/Desk Insight Gallery.app"
            ]
        );
    }

    #[test]
    fn an_app_may_take_a_typo_in_any_word() {
        let index = sample();
        let apps = applications(&["Visual Studio Code", "Safari"]);
        assert_eq!(
            ranked(&index, &apps, "visual cdoe"),
            ["/Applications/Visual Studio Code.app"]
        );
        // No app matches "plan", so none takes a typo for "safarri".
        assert!(ranked(&index, &apps, "safarri plan").is_empty());
    }

    #[test]
    fn a_newer_ticket_cancels_a_search_only_apps_answer() {
        let index = sample();
        let apps = applications(&["Safari"]);
        let searcher = Searcher::with_threads(2);
        let old = searcher.ticket();
        let _new = searcher.ticket();
        assert_eq!(searcher.search(&index, &apps, "safari", old, 10), None);
    }

    #[test]
    fn an_app_wins_a_full_tie() {
        let app = Hit {
            item: Item::App(9),
            score: 50,
            name_len: 6,
        };
        let entry = Hit {
            item: Item::Entry(0),
            ..app
        };
        assert!(order(&app, &entry).is_lt());
        let mut hits = vec![entry, app];
        keep_best(&mut hits, 1);
        assert_eq!(hits, [app]);
    }

    #[test]
    fn every_word_must_be_in_the_app_name() {
        let index = sample();
        let apps = applications(&["Visual Studio Code", "Xcode", "Safari"]);
        assert_eq!(
            ranked(&index, &apps, "visual code"),
            ["/Applications/Visual Studio Code.app"]
        );
        assert_eq!(
            ranked(&index, &apps, "code !visual"),
            ["/Applications/Xcode.app"]
        );
        assert!(ranked(&index, &apps, "safari readme").is_empty());
        assert_eq!(
            ranked(&index, &apps, "safari"),
            ["/Applications/Safari.app"]
        );
        // Files still need their words; an app matching one does not bring them in.
        assert_eq!(
            ranked(&index, &apps, "other readme"),
            ["/home/other/README.md"]
        );
    }

    #[test]
    fn an_app_holding_the_word_leaves_files_no_typo() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("safri.txt", root, false);
        let index = builder.finish();
        assert_eq!(paths(&index, "safari"), ["/home/safri.txt"]);
        let apps = applications(&["Safari"]);
        assert_eq!(
            ranked(&index, &apps, "safari"),
            ["/Applications/Safari.app"]
        );
    }

    #[test]
    fn an_app_may_take_one_typo_when_nothing_holds_the_word() {
        let index = sample();
        let apps = applications(&["Safari", "Calendar"]);
        assert_eq!(
            ranked(&index, &apps, "safarri"),
            ["/Applications/Safari.app"]
        );
        assert_eq!(
            ranked(&index, &apps, "safarri !cal"),
            ["/Applications/Safari.app"]
        );
        assert!(ranked(&index, &apps, "^safarri").is_empty());
        assert!(ranked(&index, &apps, "safarri !saf").is_empty());
        // A file holds "plan" letter for letter, so no app gets a typo for it.
        assert_eq!(ranked(&index, &apps, "plan"), ["/home/other/plan.md"]);
    }

    #[test]
    fn an_app_in_the_home_folder_shows_once() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let folder = builder.push("Applications", root, true);
        builder.push("Gmail.app", folder, true);
        builder.push("Gmail notes.txt", folder, false);
        let elsewhere = builder.push("old", root, true);
        builder.push("Gmail.app", elsewhere, true);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &["Gmail"]);
        assert_eq!(
            ranked(&index, &apps, "gmail"),
            [
                "/home/Applications/Gmail.app",
                "/home/old/Gmail.app",
                "/home/Applications/Gmail notes.txt"
            ]
        );
        // Outside the index, nothing is left out.
        let apps = applications(&["Gmail"]);
        assert_eq!(ranked(&index, &apps, "gmail").len(), 4);
    }

    #[test]
    fn a_bundle_is_left_out_only_when_its_app_is_found() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let folder = builder.push("Applications", root, true);
        builder.push("Gmail.app", folder, true);
        builder.push("notes.app", folder, false);
        builder.push(".Hidden.app", folder, true);
        builder.push("Broken.app", folder, false);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &["Gmail"]);
        // The app's name lacks ".app" and the folder's name, so only the bundle is found.
        for query in ["gmail.app", "applications gmail"] {
            assert_eq!(
                ranked(&index, &apps, query),
                ["/home/Applications/Gmail.app"],
                "{query}"
            );
        }
        // Bundles the app list skipped are files like any other.
        for (query, path) in [
            ("notes", "/home/Applications/notes.app"),
            ("hidden", "/home/Applications/.Hidden.app"),
            ("broken", "/home/Applications/Broken.app"),
        ] {
            assert_eq!(ranked(&index, &apps, query), [path], "{query}");
        }
    }

    #[test]
    fn every_limit_keeps_the_head_when_a_bundle_outranks_its_app() {
        // "a" sits inside Hat but starts the "app" of Hat.app, so one folder down the bundle
        // ranks above the app, and so do files a little weaker than the bundle.
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home/Applications", NO_PARENT, true);
        let names = ["Gmail", "Hat", "Mail", "Slack"];
        for name in names {
            builder.push(&format!("{name}.app"), root, true);
        }
        let mut parent = root;
        for depth in 0..3 {
            for name in ["xa", "x-a", "x-a-long", "a", "x a"] {
                builder.push(&format!("{name}{depth}"), parent, false);
            }
            parent = builder.push(&format!("d{depth}"), parent, true);
        }
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &names);
        let searcher = Searcher::with_threads(2);
        for query in ["a", "ap", "l", "p"] {
            let all = searcher
                .search(&index, &apps, query, searcher.ticket(), 1_000)
                .unwrap();
            for limit in 1..=all.len() {
                let few = searcher
                    .search(&index, &apps, query, searcher.ticket(), limit)
                    .unwrap();
                assert_eq!(few, all[..limit], "{query:?} limit {limit}");
            }
        }
    }

    #[test]
    fn an_app_folder_as_the_index_root_shows_each_app_once() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home/Applications", NO_PARENT, true);
        builder.push("Gmail.app", root, true);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &["Gmail"]);
        assert_eq!(
            ranked(&index, &apps, "gmail"),
            ["/home/Applications/Gmail.app"]
        );

        let mut builder = IndexBuilder::new();
        let root = builder.push("/", NO_PARENT, true);
        let applications = builder.push("Applications", root, true);
        let utilities = builder.push("Utilities", applications, true);
        builder.push("Terminal.app", utilities, true);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/Applications/Utilities"), &["Terminal"]);
        assert_eq!(
            ranked(&index, &apps, "terminal"),
            ["/Applications/Utilities/Terminal.app"]
        );
    }

    #[test]
    fn apps_in_several_folders_show_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("Applications");
        std::fs::create_dir_all(top.join("Xylophone.app")).unwrap();
        std::fs::create_dir_all(top.join("Utilities/Yodel.app")).unwrap();
        let apps = Apps::scan_folders(vec![top]);
        assert_eq!(apps.len(), 2);
        let mut builder = IndexBuilder::new();
        let root = builder.push(dir.path().to_str().unwrap(), NO_PARENT, true);
        let applications = builder.push("Applications", root, true);
        builder.push("Xylophone.app", applications, true);
        let utilities = builder.push("Utilities", applications, true);
        builder.push("Yodel.app", utilities, true);
        let index = builder.finish();
        for query in ["xylophone", "yodel"] {
            let searcher = Searcher::with_threads(1);
            let hits = searcher
                .search(&index, &apps, query, searcher.ticket(), 10)
                .unwrap();
            assert_eq!(hits.len(), 1, "{query}");
            assert!(matches!(hits[0].item, Item::App(_)), "{query}");
        }
    }

    #[test]
    fn apps_are_found_with_an_empty_index() {
        let index = IndexBuilder::new().finish();
        let mut builder = IndexBuilder::new();
        builder.push("/home", NO_PARENT, true);
        let root_only = builder.finish();
        let apps = applications(&["Maps", "Mail"]);
        for index in [&index, &root_only] {
            assert_eq!(ranked(index, &apps, "maps"), ["/Applications/Maps.app"]);
            assert_eq!(ranked(index, &apps, "ma").len(), 2);
            assert!(ranked(index, &Apps::default(), "maps").is_empty());
        }
    }
}
