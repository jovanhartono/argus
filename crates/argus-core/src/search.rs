//! Search: the letter check, fuzzy matching per word, and dropping searches nobody wants any more.

use crate::apps::{AppId, Apps};
use crate::frecency::{Boosts, Frecency};
use crate::index::{ALL_LETTERS, EntryId, Index, NameId, letter, letters, symbols};
use crate::siblings::NO_FOLDER;
use crate::within::Within;
use frizbee::{CaseMatching, Config, Match, Matcher, Matching, Pattern, Scoring, SortStrategy};
use rayon::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Each chunk is one job, and each job first checks that its search is still wanted.
const NAME_CHUNK: usize = 16_384;
/// Names handed to the fuzzy matcher at once. Its lists stay small enough that the memory macOS
/// keeps after a search is a few hundred KB, not megabytes.
const MATCH_BATCH: usize = 1024;
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
    /// The words in each folder's path, by its place in `Index::folders`.
    folder_words: Vec<u8>,
    /// One bit per distinct name a result could still need, for matching common words last.
    possible: Vec<AtomicU64>,
    /// One bit per folder, by place, already climbed while marking `possible`.
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
        self.search_with(index, apps, None, query, ticket, limit, &Boosts::default())
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
        self.search_with(
            index,
            apps,
            within,
            query,
            ticket,
            limit,
            &Boosts::default(),
        )
    }

    /// `frecency.boosts` on the search threads, so a rescan filling rayon's shared threads
    /// never holds up the next search for seconds.
    pub fn boosts(&self, frecency: &Frecency, index: &Index, apps: &Apps, now: u64) -> Boosts {
        self.pool.install(|| frecency.boosts(index, apps, now))
    }

    /// As `search_in`, with each file's and app's boost from `Searcher::boosts` added to its
    /// score.
    #[expect(clippy::too_many_arguments, reason = "one search's inputs")]
    pub fn search_with(
        &self,
        index: &Index,
        apps: &Apps,
        within: Option<&Within>,
        query: &str,
        ticket: Ticket,
        limit: usize,
        boosts: &Boosts,
    ) -> Option<Vec<Hit>> {
        let stale = || self.tickets.is_stale(ticket);
        let mut turn = self.turn.lock().unwrap();
        if stale() {
            return None;
        }
        let scratch = &mut *turn;
        self.pool.install(|| {
            let hits = run(index, apps, query, limit, within, boosts, scratch, &stale)?;
            (!stale()).then_some(hits)
        })
    }
}

#[expect(clippy::too_many_arguments, reason = "one search's inputs")]
fn run(
    index: &Index,
    apps: &Apps,
    query: &str,
    limit: usize,
    within: Option<&Within>,
    boosts: &Boosts,
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
    for hit in &mut app_hits {
        hit.score = hit.score.saturating_add(boosts.get(hit.item));
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
    // "synergy readme" ranks synergy-core/README.md above a long name that scatters the letters
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
            let mut hits = best(
                index,
                within,
                scratch,
                wanted,
                unwanted,
                &folder_bonus,
                wide,
                boosts,
                stale,
            )?;
            // Every entry sits at least one folder deep, which costs a plain name a little more.
            let ceiling = u32::from(ceiling);
            if hits.len() == wide && hits[wide - 1].score > ceiling.saturating_sub(DEPTH_PENALTY) {
                // A file opened often may still climb in by its boost when its name is plain
                // too. Scoring just those few names is far cheaper than every plain name.
                let weakest = hits[wide - 1].score;
                let needed = letters(&words[0].needle);
                let climbers: Vec<(EntryId, NameId, u32)> = boosts
                    .iter()
                    .filter_map(|(entry, boost)| {
                        let at = entry as usize;
                        let name = *index.entry_name.get(at)?;
                        if within.is_some_and(|within| !within.holds(index.parent(entry))) {
                            return None;
                        }
                        let plain = needed & !index.name_letters[name as usize] == 0
                            && index.word_starts(name as usize) & needed == 0;
                        let depth = DEPTH_PENALTY * u32::from(index.depth(entry));
                        let could = plain && ceiling.saturating_sub(depth) + boost >= weakest;
                        could.then_some((entry, name, boost))
                    })
                    .collect();
                if !climbers.is_empty() {
                    reset(&mut scratch.possible, index.distinct_names());
                    for &(_, name, _) in &climbers {
                        mark(&scratch.possible, name as usize);
                    }
                    let pass = Pass::Possible;
                    match_names(index, &words, names, wanted, wanted, pass, scratch, stale)?;
                    let climbers = climbers.iter().map(|&(entry, _, boost)| (entry, boost));
                    let (bonus, scratch) = (&folder_bonus, &*scratch);
                    push_boosted(
                        index, within, scratch, wanted, unwanted, bonus, climbers, &mut hits,
                    );
                    keep_best(&mut hits, wide);
                }
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
        boosts,
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
    boosts: &Boosts,
    stale: &(impl Fn() -> bool + Sync),
) -> Option<Vec<Hit>> {
    let use_folders = uses_folders(wanted, unwanted);
    let Scratch {
        name_words,
        name_score,
        folder_words,
        ..
    } = scratch;
    let siblings = &index.siblings;
    let entries = within.map_or(0..index.len(), |within| within.entries.clone());
    let start = entries.start;
    let chunks: Vec<Vec<Hit>> = index.entry_name[entries]
        .par_chunks(ENTRY_CHUNK)
        .enumerate()
        .map(|(chunk, names)| {
            if stale() {
                return None;
            }
            let first = start + chunk * ENTRY_CHUNK;
            let mut hits = Vec::with_capacity((2 * limit).min(names.len()));
            // Once `limit` hits are kept, anything ranking below the weakest is skipped,
            // so a one-letter search does not collect two million hits to keep 50.
            let mut weakest: Option<Hit> = None;
            let mut runs = siblings.cursor(first as EntryId);
            for (entry, &name) in (first as EntryId..).zip(names) {
                let own = name_words[name as usize];
                if own & wanted == 0 {
                    continue;
                }
                let run = runs.run(entry);
                let folder = siblings.folders[run];
                if folder == NO_FOLDER {
                    continue;
                }
                if within.is_some_and(|within| !within.holds(index.folders[folder as usize])) {
                    continue;
                }
                let mut path = own;
                if use_folders {
                    path |= folder_words[folder as usize];
                }
                if path & wanted != wanted || path & unwanted != 0 {
                    continue;
                }
                let score = (u32::from(name_score[name as usize])
                    + folder_bonus[usize::from(wanted & !own)])
                .saturating_sub(DEPTH_PENALTY * u32::from(siblings.depths[run]));
                if weakest.is_some_and(|weakest| score < weakest.score) {
                    continue;
                }
                let hit = Hit {
                    item: Item::Entry(entry),
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
    if !boosts.is_empty() {
        // Only a boosted entry can climb past the best `limit` hits found without boosts, so
        // the result is still exactly the best.
        for hit in &mut hits {
            hit.score = hit.score.saturating_add(boosts.get(hit.item));
        }
        push_boosted(
            index,
            within,
            scratch,
            wanted,
            unwanted,
            folder_bonus,
            boosts.iter(),
            &mut hits,
        );
        hits.sort_unstable_by_key(|hit| hit.item);
        hits.dedup_by_key(|hit| hit.item);
        keep_best(&mut hits, limit);
    }
    Some(hits)
}

/// Adds each `boosted` entry that matches, scored as `best`'s loop scores every entry. The loop
/// keeps its own copy: calling this from it cost 1.3% more instructions per search.
#[expect(
    clippy::too_many_arguments,
    reason = "one search's state, split for the borrow checker"
)]
fn push_boosted(
    index: &Index,
    within: Option<&Within>,
    scratch: &Scratch,
    wanted: u8,
    unwanted: u8,
    folder_bonus: &[u32; 1 << MAX_WORDS],
    boosted: impl Iterator<Item = (EntryId, u32)>,
    hits: &mut Vec<Hit>,
) {
    let use_folders = uses_folders(wanted, unwanted);
    for (entry, boost) in boosted {
        // Boosts worked out for an older list may point past this one.
        let Some(&name) = index.entry_name.get(entry as usize) else {
            continue;
        };
        let own = scratch.name_words[name as usize];
        if own & wanted == 0 {
            continue;
        }
        let run = index.siblings.run_of(entry);
        let folder = index.siblings.folders[run];
        if folder == NO_FOLDER {
            continue;
        }
        if within.is_some_and(|within| !within.holds(index.folders[folder as usize])) {
            continue;
        }
        let mut path = own;
        if use_folders {
            path |= scratch.folder_words[folder as usize];
        }
        if path & wanted != wanted || path & unwanted != 0 {
            continue;
        }
        let score = (u32::from(scratch.name_score[name as usize])
            + folder_bonus[usize::from(wanted & !own)])
        .saturating_sub(DEPTH_PENALTY * u32::from(index.siblings.depths[run]));
        hits.push(Hit {
            item: Item::Entry(entry),
            score: score.saturating_add(boost),
            name_len: index.name_len(name),
        });
    }
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
                let needle = &words[w].needle;
                let (score, holds) = points(found, name, needle, 0, true, &config.scoring);
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
fn points(
    found: Match,
    name: &str,
    word: &str,
    floor: u16,
    app: bool,
    scoring: &Scoring,
) -> (u16, bool) {
    let score = found.score;
    let how = if score < floor {
        Held::No
    } else {
        held_in(name, word, app)
    };
    match how {
        how @ (Held::Whole | Held::First | Held::Alone) => {
            let exact = match how {
                Held::Whole if !found.exact => scoring.exact_match_bonus,
                // One under the whole name, so "xcode" still puts Xcode above Xcode-beta, and
                // one over a lark folder at home, which loses `DEPTH_PENALTY` for its depth.
                Held::First => scoring.exact_match_bonus.saturating_sub(1),
                _ => 0,
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
    let needed_symbols: Vec<u8> = words.iter().map(|word| symbols(&word.needle)).collect();
    let dot = symbols(".");
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
                let sides: Vec<Sides> = (0..words.len())
                    .map(|w| Sides::new(&words[w], typos(w)))
                    .collect();
                (matchers, sides, Vec::new(), Vec::new())
            },
            |(matchers, sides, ids, texts), (chunk, (bits, scores))| {
                if stale() {
                    return None;
                }
                let first = chunk * NAME_CHUNK;
                let (mut matched, mut held) = (0u8, 0u8);
                for (w, (matcher, sides)) in matchers.iter_mut().zip(sides.iter_mut()).enumerate() {
                    if active >> w & 1 == 0 {
                        continue;
                    }
                    // "zshrc" should find .zshrc as readily as zshrc.bak; typing ".zsh" keeps the dot.
                    let keep_dot = words[w].needle.starts_with('.');
                    let one_side = splits_at_dot(&words[w]);
                    let end = first + bits.len();
                    for batch in (first..end).step_by(MATCH_BATCH) {
                        ids.clear();
                        texts.clear();
                        for n in batch..end.min(batch + MATCH_BATCH) {
                            if only_possible && !is_marked(possible, n) {
                                continue;
                            }
                            if names.is_some_and(|names| {
                                (names[n / 64] >> (n % 64) & 1 == 1) == outside
                            }) {
                                continue;
                            }
                            let have = index.name_letters[n];
                            let missing = (needed[w] & !have).count_ones();
                            if missing > u32::from(typos(w)) {
                                continue;
                            }
                            if needed_symbols[w] != 0 {
                                let have = index.name_symbols[n];
                                let symbols = (needed_symbols[w] & !have).count_ones();
                                if missing + symbols > u32::from(typos(w)) {
                                    continue;
                                }
                            }
                            let starts = || index.word_starts(n);
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
                                && have & ALL_LETTERS != ALL_LETTERS
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
                            let text = texts[found.index as usize];
                            let word = &words[w];
                            let n = ids[found.index as usize];
                            // A name with no dot has no extension, known without reading it.
                            let dotted = index.name_symbols[first + n] & dot != 0;
                            if one_side && dotted && !beside_extension(text, word, matcher, sides) {
                                continue;
                            }
                            bits[n] |= 1 << w;
                            matched |= 1 << w;
                            if wanted >> w & 1 == 1 {
                                let needle = &word.needle;
                                let (score, holds) =
                                    points(found, text, needle, floors[w], false, &config.scoring);
                                held |= u8::from(holds) << w;
                                scores[n] = scores[n].saturating_add(score);
                            }
                        }
                    }
                }
                Some((matched, held))
            },
        )
        .try_reduce(|| (0, 0), |a, b| Some((a.0 | b.0, a.1 | b.1)))
}

/// Whether a word's letters must stay on one side of a name's extension. A word with a dot in
/// it, or one matched letter for letter by `^`, `$` or `'`, already says where it goes.
fn splits_at_dot(word: &Pattern) -> bool {
    word.config.matching.is_none() && !word.needle.contains('.')
}

/// A name's text before and after the dot of its extension, when it has one. Only letters and
/// digits make an extension, so "Mr. Robot" and "node-v20.11.1-darwin-arm64" have none.
fn extension(text: &str) -> Option<(&str, &str)> {
    let dot = text.rfind('.')?;
    let tail = &text[dot + 1..];
    (dot > 0 && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| (&text[..dot], tail))
}

/// What one word needs to check names against its extension rule.
struct Sides {
    typos: u16,
    /// An ASCII word with no typo, checked by its letters alone.
    plain: bool,
    /// A capital typed, so every piece minds case as the whole word does.
    respect: bool,
    letters: u64,
    /// The matchers for each way of ending the word inside an extension, made the first
    /// time a name needs them.
    cuts: Option<Vec<Cut>>,
}

impl Sides {
    fn new(word: &Pattern, typos: u16) -> Self {
        let needle = word.needle.as_str();
        Self {
            typos,
            plain: typos == 0 && needle.is_ascii(),
            respect: needle.chars().any(char::is_uppercase),
            letters: if needle.is_ascii() {
                letters(needle)
            } else {
                ALL_LETTERS
            },
            cuts: None,
        }
    }
}

/// The word cut in two: the head before the dot, the end in the extension. A word allowed a
/// typo spends it in one part: the head with the typo and the end letter for letter, or
/// the other way round.
struct Cut {
    /// The head, with the word's typo.
    head: Matcher,
    /// The end letter for letter: the extension's first letters, or a run of three or more.
    end: Matcher,
    /// Two letters or more, so the end may also open the extension at its second letter,
    /// the first left out: "nextconfigjs" for next.config.mjs.
    second: bool,
    /// For a word allowed a typo: the head letter for letter, and the end with the typo.
    loose: Option<Loose>,
}

struct Loose {
    head: Matcher,
    end: Matcher,
    /// The end's first letter, which must open the extension unless the end is long.
    first: char,
    /// Four letters or more, enough to stand out wherever they sit in the extension:
    /// "shopworkspcae" for Shop.xcworkspace.
    long: bool,
    /// The end after its first letter, which may be the typo itself: "reportodf".
    rest: String,
    /// The end with its first two letters swapped back, the commonest slip: "invoicedpf".
    swapped: Option<String>,
}

/// Whether a name frizbee matched still fits the word with no stray letter of it taken from
/// the extension: "cmux" is not credit-mutation.tsx, whose x only ".tsx" holds. The word may
/// end on the extension's first letters, on two or more after its first ("nextconfigjs" for
/// next.config.mjs), or on three or more of its letters in a row, so "reportpdf" still finds
/// report.pdf and "shopworkspace" Shop.xcworkspace. A word allowed a
/// typo may take it in either part: "designpfd" still finds design.pdf, "mainfs" main.rs.
fn beside_extension(text: &str, word: &Pattern, matcher: &mut Matcher, sides: &mut Sides) -> bool {
    let needle = word.needle.as_str();
    // "te" fits hundreds of thousands of names, nearly all of them before the dot, so one
    // pass over the letters settles most names.
    if sides.plain
        && let Some(dot) = text.bytes().rposition(|b| b == b'.')
        && letters_end(needle, &text[..dot], sides.respect).is_some()
    {
        return true;
    }
    let Some((stem, tail)) = extension(text) else {
        return true;
    };
    // Most extensions share no letter with the word, like ".pdf" for "notes", so no match
    // can reach into them. A typo there only stands for a letter the word lacks.
    if sides.letters & letters(tail) == 0 {
        return true;
    }
    if sides.plain && stem.is_ascii() {
        return fits_by_letters(stem, tail, needle, sides.respect);
    }
    fits_by_frizbee(stem, tail, word, matcher, sides)
}

/// Where the ASCII word's letters, found in order as early as they come, end in `text`.
fn letters_end(word: &str, text: &str, respect: bool) -> Option<usize> {
    fn end(word: &[u8], text: &[u8], same: impl Fn(u8, u8) -> bool) -> Option<usize> {
        let mut at = 0;
        for &w in word {
            at += text[at..].iter().position(|&t| same(t, w))? + 1;
        }
        Some(at)
    }
    let (word, text) = (word.as_bytes(), text.as_bytes());
    // A word with no capital is all lower case already.
    if respect {
        end(word, text, |t, w| t == w)
    } else {
        end(word, text, |t, w| t.to_ascii_lowercase() == w)
    }
}

/// `beside_extension` for an ASCII word with no typo, where frizbee's fuzzy match is the
/// word's letters in order.
fn fits_by_letters(stem: &str, tail: &str, needle: &str, respect: bool) -> bool {
    let same = |a: &[u8], b: &[u8]| {
        if respect {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    };
    let tail_bytes = tail.as_bytes();
    let opens_at = |at: usize, end: &[u8]| {
        tail_bytes.len() >= at + end.len() && same(&tail_bytes[at..at + end.len()], end)
    };
    in_order(needle, stem, respect)
        || in_order(needle, tail, respect)
        || (1..needle.len()).any(|cut| {
            let end = &needle.as_bytes()[cut..];
            let opens = opens_at(0, end) || (end.len() >= 2 && opens_at(1, end));
            let run = end.len() >= 3 && tail_bytes.windows(end.len()).any(|w| same(w, end));
            (opens || run) && in_order(&needle[..cut], stem, respect)
        })
}

/// `beside_extension` for any word, asking frizbee about each part.
fn fits_by_frizbee(
    stem: &str,
    tail: &str,
    word: &Pattern,
    matcher: &mut Matcher,
    sides: &mut Sides,
) -> bool {
    let needle = word.needle.as_str();
    if matcher.match_one(stem, 0).is_some() || matcher.match_one(tail, 0).is_some() {
        return true;
    }
    let (typos, respect) = (sides.typos, sides.respect);
    let cuts = sides.cuts.get_or_insert_with(|| {
        let config = matcher_config();
        let casing = if respect {
            CaseMatching::Respect
        } else {
            CaseMatching::Ignore
        };
        let piece = |text: &str| Pattern::new(text, word.config.clone()).casing(Some(casing));
        needle
            .char_indices()
            .skip(1)
            .map(|(cut, first)| {
                let end = &needle[cut..];
                let run = if end.chars().count() >= 3 {
                    Matching::Substring
                } else {
                    Matching::Prefix
                };
                let head = &needle[..cut];
                let loose = (typos > 0).then(|| Loose {
                    head: Matcher::new(piece(head).max_typos(Some(0)), &config),
                    end: Matcher::new(piece(end).max_typos(Some(typos)), &config),
                    first,
                    long: end.chars().count() >= 4,
                    rest: end[first.len_utf8()..].to_owned(),
                    swapped: {
                        let mut letters = end.chars();
                        let (a, b) = (letters.next(), letters.next());
                        a.zip(b)
                            .map(|(a, b)| [b, a].into_iter().chain(letters).collect())
                    },
                });
                Cut {
                    head: Matcher::new(piece(head).max_typos(Some(typos)), &config),
                    end: Matcher::new(piece(end).matching(Some(run)), &config),
                    second: end.chars().count() >= 2,
                    loose,
                }
            })
            .collect()
    });
    let same = |a: char, b: char| {
        if respect {
            a == b
        } else {
            a.eq_ignore_ascii_case(&b)
        }
    };
    let mut after = tail.chars();
    let opening = after.next();
    let after = after.as_str();
    cuts.iter_mut().any(|cut| {
        let end = cut.end.match_one(tail, 0).is_some()
            || (cut.second && cut.end.match_one(after, 0).is_some());
        if end && cut.head.match_one(stem, 0).is_some() {
            return true;
        }
        cut.loose.as_mut().is_some_and(|loose| {
            let opens = opening.is_some_and(|t| same(t, loose.first));
            let near = (opens || loose.long) && loose.end.match_one(tail, 0).is_some();
            let opens_with = |text: &str, letters: &str| {
                let mut text = text.chars();
                letters
                    .chars()
                    .all(|l| text.next().is_some_and(|t| same(l, t)))
            };
            let replaced = opening.is_some() && opens_with(after, &loose.rest);
            let swapped = loose
                .swapped
                .as_deref()
                .is_some_and(|swapped| opens_with(tail, swapped));
            (near || replaced || swapped) && loose.head.match_one(stem, 0).is_some()
        })
    })
}

/// Whether `text` holds the ASCII word's letters in order, which is a match for frizbee when
/// the word allows no typo.
fn in_order(word: &str, text: &str, respect: bool) -> bool {
    letters_end(word, text, respect).is_some()
}

/// How a name holds a typed word, letter for letter: "nib" is the whole of "NIB.pdf" bar its
/// extension, stands alone in "my NIB.pdf", sits inside "nibble.go", and is not held by "nimble.go".
enum Held {
    No,
    Inside,
    Alone,
    /// The first word of an app's name, as "lark" in LarkSuite.
    First,
    Whole,
}

/// People start an app by its first word, "lark" for LarkSuite, so in an app's name that word
/// counts almost as much as the whole name, and a capital after a lower-case letter starts a word. Not in file names,
/// where hashes like "sLaR.js" would gain at random. Nor for one to three letters, a quick
/// jump like "go" for ~/go rather than GoLand.
fn held_in(name: &str, word: &str, app: bool) -> Held {
    let (name, word) = (name.as_bytes(), word.as_bytes());
    let app = app && word.len() >= 4;
    let camel =
        |at: usize| app && name[at - 1].is_ascii_lowercase() && name[at].is_ascii_uppercase();
    let mut result = Held::No;
    // One letter matches most names; checking each for it is time with nothing to gain.
    if word.len() < 2 || word.len() > name.len() {
        return result;
    }
    for (i, window) in name.windows(word.len()).enumerate() {
        if window.eq_ignore_ascii_case(word) {
            let end = i + word.len();
            let starts = i == 0 || !name[i - 1].is_ascii_alphanumeric() || camel(i);
            let ends = end == name.len() || !name[end].is_ascii_alphanumeric() || camel(end);
            if starts && ends {
                return match &name[end..] {
                    [] if i == 0 => Held::Whole,
                    [b'.', extension @ ..] if i == 0 && !extension.contains(&b'.') => Held::Whole,
                    _ if i == 0 && app => Held::First,
                    _ => Held::Alone,
                };
            }
            result = Held::Inside;
        }
    }
    let starts = |at: usize| {
        name[at].is_ascii_alphanumeric()
            && (at == 0 || !name[at - 1].is_ascii_alphanumeric() || camel(at))
    };
    let ends = |at: usize| at == name.len() || !name[at].is_ascii_alphanumeric() || camel(at);
    // "vscode" is Visual Studio Code typed as V·S·Code: like a whole word, not a typo.
    if app && on_word_starts(name, word, 0, &starts, &ends) {
        return Held::Alone;
    }
    result
}

/// Whether `word` spells the app's words in order from the first, each by its initial or in
/// full, the last by at least two letters: "vscod" on the way to "vscode". Only the last may be
/// cut short, so "desing" is not Des·In·G of Desk Insight Gallery, and "colors" is not
/// Color·S of ColorSync.
fn on_word_starts(
    name: &[u8],
    word: &[u8],
    from: usize,
    starts: &impl Fn(usize) -> bool,
    ends: &impl Fn(usize) -> bool,
) -> bool {
    let Some(at) = (from..name.len()).find(|&at| starts(at)) else {
        return false;
    };
    let end = (at + 1..=name.len())
        .find(|&end| ends(end))
        .unwrap_or(name.len());
    let whole = &name[at..end];
    let opens = |piece: &[u8]| {
        piece.len() <= whole.len() && whole[..piece.len()].eq_ignore_ascii_case(piece)
    };
    (from > 0 && word.len() >= 2 && opens(word))
        || (word.len() > 1
            && opens(&word[..1])
            && on_word_starts(name, &word[1..], end, starts, ends))
        || (whole.len() > 1
            && word.len() > whole.len()
            && opens(&word[..whole.len()])
            && on_word_starts(name, &word[whole.len()..], end, starts, ends))
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
    reset(walked, index.folders.len());
    let (possible, walked) = (&*possible, &*walked);
    let siblings = &index.siblings;
    let entries = within.map_or(0..index.len(), |within| within.entries.clone());
    let start = entries.start;
    index.entry_name[entries]
        .par_chunks(ENTRY_CHUNK)
        .enumerate()
        .try_for_each(|(chunk, names)| {
            if stale() {
                return None;
            }
            let first = start + chunk * ENTRY_CHUNK;
            for (entries, folder) in index.runs_in(first..first + names.len()) {
                if folder == NO_FOLDER
                    || within.is_some_and(|within| !within.holds(index.folders[folder as usize]))
                {
                    continue;
                }
                let above = folder_words[folder as usize];
                for &name in &names[entries.start - first..entries.end - first] {
                    let path = name_words[name as usize] | above;
                    if path & rare != rare || path & unwanted != 0 {
                        continue;
                    }
                    mark(possible, name as usize);
                    let mut slot = folder;
                    // A folder someone already walked through has its own folders above marked.
                    while slot != NO_FOLDER && !mark(walked, slot as usize) {
                        let folder = index.folders[slot as usize];
                        mark(possible, index.entry_name[folder as usize] as usize);
                        slot = siblings.folder_parents[slot as usize];
                    }
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
    folder_words.resize(index.folders.len(), 0);
    let parents = &index.siblings.folder_parents;
    for (slot, (&folder, &parent)) in index.folders.iter().zip(parents).enumerate() {
        if slot % ENTRY_CHUNK == 0 && stale() {
            return None;
        }
        if parent != NO_FOLDER {
            let own = name_words[index.entry_name[folder as usize] as usize];
            folder_words[slot] = own | folder_words[parent as usize];
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
    use crate::index::{IndexBuilder, NO_PARENT, word_starts};
    use std::path::{Path, PathBuf};

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
                if floor > 0 && !matches!(held_in(name, &needle, false), Held::No) {
                    checked += 1;
                    assert!(found.score >= floor, "{typed:?} in {name:?}");
                }
            }
        }
        assert!(checked > 1_000);
    }

    #[test]
    fn the_letter_and_symbol_check_never_drops_a_name_frizbee_matches() {
        let mut rng = Rng(0x5a7b);
        let parts = [
            "a", "b", "3", "A", ".", "-", "_", " ", "~", "(", "é", "日", "a", ".", "-", "_",
        ];
        let typed = ["a", "b", "3", "B", ".", "-", "_", "~", "("];
        let config = matcher_config();
        let mut checked = 0;
        for _ in 0..3_000 {
            let needle = rng.text(&typed, 5);
            let typos = rng.below(2) as u16;
            let Some(word) = Pattern::parse_query(&needle).pop() else {
                continue;
            };
            let word = positive(word).max_typos(Some(typos));
            let keep_dot = word.needle.starts_with('.');
            let (needed, needed_symbols) = (letters(&word.needle), symbols(&word.needle));
            let names: Vec<String> = (0..40).map(|_| rng.name(&parts, 12)).collect();
            let texts: Vec<&str> = names
                .iter()
                .map(|name| match name.strip_prefix('.') {
                    Some(hidden) if !keep_dot => hidden,
                    _ => name,
                })
                .collect();
            for found in Matcher::new(word.clone(), &config).match_list(&texts) {
                let name = &names[found.index as usize];
                let (have, _, have_symbols) = crate::index::name_fingerprint(name);
                let missing =
                    (needed & !have).count_ones() + (needed_symbols & !have_symbols).count_ones();
                assert!(missing <= u32::from(typos), "{needle:?} in {name:?}");
                checked += usize::from(needed_symbols != 0);
            }
        }
        assert!(checked > 1_000, "{checked}");
    }

    #[test]
    fn one_letter_scores_match_frizbee() {
        let mut rng = Rng(0xc0ffee);
        let names = [
            "a", "b", "c", "3", "A", "B", "C", ".", "-", "_", " ", "(", "~", "a", "c", "2",
        ];
        let config = matcher_config();
        let mut checked = 0;
        for typed in ["a", "b", "c", "3", "2", "z"] {
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
                    let held = held_in(stripped[found.index as usize], &typed, false);
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
        boosts: &Boosts,
    ) -> (Vec<Hit>, usize) {
        let mut fast = Scratch::default();
        let mut slow = Scratch {
            no_shortcuts: true,
            ..Scratch::default()
        };
        let never = &|| false;
        let hits = run(index, apps, query, limit, None, boosts, &mut fast, never).unwrap();
        let expected = run(index, apps, query, limit, None, boosts, &mut slow, never).unwrap();
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
        let (mut early_stops, mut boosted_stops) = (0, 0);
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
            // Files and apps opened from anywhere, the plain names the shortcuts skip included.
            let boosts = (0..rng.below(40))
                .map(|_| (rng.below(index.len()) as EntryId, 1 + rng.below(48) as u32))
                .collect::<Boosts>()
                .with_apps((0..names.len() as AppId).map(|app| (app, rng.below(49) as u32)));
            for query in [
                "c", "3", "38", "x8", "38x", "'c", "'38", "^c", "c$", "C", ".c", "c a", "38 c",
                "a !b", "cab", "cab 38x",
            ] {
                for limit in [1, 3, 10, 40, 1_000] {
                    let none = Boosts::default();
                    let stops = with_and_without_shortcuts(&index, &apps, query, limit, &none).1;
                    early_stops += stops;
                    let stops = with_and_without_shortcuts(&index, &apps, query, limit, &boosts).1;
                    boosted_stops += stops;
                }
            }
        }
        assert!(early_stops > 100, "{early_stops}");
        assert!(boosted_stops > 100, "{boosted_stops}");
    }

    fn is_inside(index: &Index, entry: EntryId, folder: EntryId) -> bool {
        let mut at = index.parent(entry);
        while at != NO_PARENT {
            if at == folder {
                return true;
            }
            at = index.parent(at);
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
        boosts: &Boosts,
    ) -> (usize, Scratch) {
        let within = Within::new(index, folder);
        let every = index.len() + apps.len();
        let never = &|| false;
        let all = run(
            index,
            apps,
            query,
            every,
            None,
            boosts,
            &mut Scratch::default(),
            never,
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
            let found = run(
                index,
                apps,
                query,
                limit,
                within,
                boosts,
                &mut scratch,
                never,
            );
            assert_eq!(
                found.unwrap(),
                expected,
                "{query:?} in {folder} limit {limit}"
            );
            let found = run(index, apps, query, limit, within, boosts, &mut slow, never).unwrap();
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
        let mut boosted_stops = 0;
        let mut check = |index: &Index, apps: &Apps, folder: EntryId, query: &str| {
            let none = Boosts::default();
            let (kept, scratch) = assert_scoped_is_filtered(index, apps, folder, query, &none);
            found += kept;
            early_stops += scratch.early_stops;
            later += scratch.later_passes;
            outside_checks += scratch.outside_checks;
            // Files opened inside and outside the folder, plain names included.
            let mut rng = Rng((index.len() as u64) << 32 | u64::from(folder) << 1 | 1);
            let boosts: Boosts = (0..rng.below(40))
                .map(|_| (rng.below(index.len()) as EntryId, 1 + rng.below(48) as u32))
                .collect();
            let (_, scratch) = assert_scoped_is_filtered(index, apps, folder, query, &boosts);
            boosted_stops += scratch.early_stops;
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
        assert!(boosted_stops > 100, "{boosted_stops}");
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
            let none = Boosts::default();
            let (kept, _) = assert_scoped_is_filtered(&index, apps, folder, query, &none);
            let mut scratch = Scratch::default();
            let within = Some(Within::new(&index, folder));
            let within = within.as_ref();
            run(
                &index,
                apps,
                query,
                10,
                within,
                &none,
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
    fn names_on_both_sides_of_each_batch_edge_are_found() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let count = 3 * MATCH_BATCH + 7;
        for i in 0..count {
            builder.push(&format!("n{i}q"), root, false);
            builder.push(&format!("other{i}"), root, false);
        }
        let index = builder.finish();
        let searcher = Searcher::with_threads(2);
        let hits = searcher
            .search(&index, &Apps::default(), "nq", searcher.ticket(), 4 * count)
            .unwrap();
        let mut found: Vec<&str> = hits
            .iter()
            .filter_map(|hit| hit.entry())
            .map(|e| index.name(e))
            .collect();
        found.sort_unstable();
        let mut expected: Vec<String> = (0..count).map(|i| format!("n{i}q")).collect();
        expected.sort_unstable();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_word_held_in_an_early_batch_turns_off_typos_for_the_later_ones() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("plan.md", root, false);
        for i in 0..2 * MATCH_BATCH {
            builder.push(&format!("other{i}"), root, false);
        }
        builder.push("plam.md", root, false);
        let index = builder.finish();
        assert_eq!(paths(&index, "plan"), ["/home/plan.md"]);

        // Each name scores across batches as it does alone.
        let searcher = Searcher::with_threads(1);
        for query in ["plan", "pl md", "other1"] {
            let hits = searcher
                .search(
                    &index,
                    &Apps::default(),
                    query,
                    searcher.ticket(),
                    index.len(),
                )
                .unwrap();
            for hit in hits {
                let name = index.name(hit.entry().unwrap());
                let mut alone = IndexBuilder::new();
                let root = alone.push("/home", NO_PARENT, true);
                alone.push(name, root, false);
                let alone = alone.finish();
                let hits = searcher.search(&alone, &Apps::default(), query, searcher.ticket(), 2);
                assert_eq!(hits.unwrap()[0].score, hit.score, "{query:?} in {name:?}");
            }
        }
    }

    #[test]
    fn runs_across_chunk_edges_find_what_runs_in_one_chunk_find() {
        // One tree, as (parent, name, folder) with parents first: two folders below the root,
        // "big" holding 70,000 entries, so its children cross entry 65,536 in a scan's order.
        let mut tree = vec![(0, "/home".to_string(), true)];
        tree.push((0, "big".to_string(), true));
        tree.push((0, "small".to_string(), true));
        for i in 0..70_000 {
            let name = format!("f{}x{}", i % 500, if i % 7 == 0 { "-old" } else { "" });
            tree.push((1, name, i % 1000 == 0));
        }
        for i in 0..2000 {
            tree.push((2, format!("f{}y", i % 300), false));
        }
        for folder in (3..70_003).step_by(1000) {
            for name in ["inner", "f12x", "plan"] {
                tree.push((folder, name.to_string(), false));
            }
        }
        let depth = |mut node: usize| {
            let mut depth = 0;
            while node != 0 {
                (node, depth) = (tree[node].0, depth + 1);
            }
            depth
        };
        let build = |order: &[usize]| {
            let mut builder = IndexBuilder::new();
            let mut entry = vec![NO_PARENT; tree.len()];
            for &node in order {
                let (parent, name, is_dir) = &tree[node];
                let parent = if node == 0 { NO_PARENT } else { entry[*parent] };
                entry[node] = builder.push(name, parent, *is_dir);
            }
            builder.finish()
        };
        let scanned = build(&(0..tree.len()).collect::<Vec<_>>());
        assert!(scanned.siblings.len() < 100);
        // The same tree with each depth's entries shuffled, so every folder's children are
        // spread over many runs.
        let mut rng = Rng(0xc4a1);
        let mut order: Vec<usize> = (0..tree.len()).collect();
        for at in (1..order.len()).rev() {
            order.swap(at, rng.below(at + 1));
        }
        order.sort_by_key(|&node| depth(node));
        let shuffled = build(&order);
        assert!(shuffled.siblings.len() > 3000);

        let found = |index: &Index, query: &str| {
            let mut scratch = Scratch::default();
            let hits = run(
                index,
                &Apps::default(),
                query,
                index.len(),
                None,
                &Boosts::default(),
                &mut scratch,
                &|| false,
            );
            let mut hits: Vec<(PathBuf, u32)> = (hits.unwrap().iter())
                .map(|hit| (index.path(hit.entry().unwrap()), hit.score))
                .collect();
            hits.sort();
            hits
        };
        for query in [
            "f12x",
            "big f12",
            "f1 !old",
            "small f2",
            "f3 x",
            "inner big",
            "f1 !big",
        ] {
            let hits = found(&scanned, query);
            assert!(!hits.is_empty(), "{query:?}");
            assert_eq!(hits, found(&shuffled, query), "{query:?}");
        }
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
        let (apps, none) = (Apps::default(), Boosts::default());
        let (hits, _) = with_and_without_shortcuts(&index, &apps, "c", 10, &none);
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
        let (apps, none) = (applications(&["Reminders"]), Boosts::default());
        let scratch = &mut Scratch::default();
        let ran = run(&index, &apps, "re", 10, None, &none, scratch, &stale);
        assert_eq!(ran, None);
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
        builder.push("sync_energy_ready_made.txt", root, false);
        let synergy = builder.push("synergy-core", root, true);
        builder.push("README.md", synergy, false);
        let index = builder.finish();
        assert_eq!(
            first(&index, "synergy readme"),
            "/home/synergy-core/README.md"
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
        let apps = Apps::default();
        assert_eq!(searcher.search(&index, &apps, "readme", old, 10), None);
    }

    #[test]
    fn a_search_inside_a_folder_gives_up_at_any_check_when_replaced() {
        let index = sample();
        let within = Some(Within::new(&index, 2));
        let within = within.as_ref();
        let (apps, none) = (applications(&["Reminders"]), Boosts::default());
        for query in ["re", "readme", "plna", "other plna", "md !plan"] {
            let checks = AtomicU64::new(0);
            let count = || checks.fetch_add(1, Ordering::Relaxed) == u64::MAX;
            let scratch = &mut Scratch::default();
            assert!(run(&index, &apps, query, 10, within, &none, scratch, &count).is_some());
            let total = checks.load(Ordering::Relaxed);
            assert!(total >= 2, "{query:?} {total}");
            assert_eq!(scratch.outside_checks, usize::from(query.contains("plna")));
            for replaced_at in 0..total {
                let checks = AtomicU64::new(0);
                let stale = || checks.fetch_add(1, Ordering::Relaxed) >= replaced_at;
                let scratch = &mut Scratch::default();
                let hits = run(&index, &apps, query, 10, within, &none, scratch, &stale);
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
        let (apps, none) = (Apps::default(), Boosts::default());
        let scratch = &mut Scratch::default();
        let ran = run(&index, &apps, "readme", 10, None, &none, scratch, &stale);
        assert_eq!(ran, None);
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

    fn boosted(index: &Index, query: &str, limit: usize, boosts: &Boosts) -> Vec<Hit> {
        boosted_with(index, &Apps::default(), query, limit, boosts)
    }

    fn boosted_with(
        index: &Index,
        apps: &Apps,
        query: &str,
        limit: usize,
        boosts: &Boosts,
    ) -> Vec<Hit> {
        let searcher = Searcher::with_threads(2);
        let ticket = searcher.ticket();
        searcher
            .search_with(index, apps, None, query, ticket, limit, boosts)
            .unwrap()
    }

    fn entries(hits: &[Hit]) -> Vec<EntryId> {
        hits.iter().filter_map(Hit::entry).collect()
    }

    #[test]
    fn a_file_opened_often_climbs_past_a_slightly_better_match() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let a = builder.push("a", root, true);
        let b = builder.push("b", a, true);
        let deep = builder.push("plan.md", b, false);
        builder.push("plan.md", root, false);
        let index = builder.finish();
        assert_eq!(first(&index, "plan"), "/home/plan.md");
        let boosts: Boosts = [(deep, 12)].into_iter().collect();
        let hits = boosted(&index, "plan", 10, &boosts);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].item, Item::Entry(deep));
        assert!(boosted(&index, "zzz", 10, &boosts).is_empty());
    }

    #[test]
    fn an_opened_file_below_the_limit_still_climbs_into_it() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for i in 0..200 {
            builder.push(&format!("plan {i}.md"), root, false);
        }
        let deep = builder.push("deep", root, true);
        let opened = builder.push("plan 7.md", deep, false);
        let index = builder.finish();
        let unboosted = boosted(&index, "plan", 5, &Boosts::default());
        assert!(!entries(&unboosted).contains(&opened));
        let boosts: Boosts = [(opened, 48)].into_iter().collect();
        let hits = boosted(&index, "plan", 5, &boosts);
        assert_eq!(hits.len(), 5);
        assert_eq!(hits[0].item, Item::Entry(opened));
        assert_eq!(hits[1..], unboosted[..4]);
    }

    #[test]
    fn a_boost_never_brings_in_what_the_search_leaves_out() {
        let index = sample();
        let boosts: Boosts = (1..index.len() as EntryId).map(|e| (e, 48)).collect();
        let found = |query: &str| {
            let hits = boosted(&index, query, 10, &boosts);
            let mut found: Vec<String> = entries(&hits)
                .into_iter()
                .map(|entry| index.path(entry).to_string_lossy().replace('\\', "/"))
                .collect();
            found.sort();
            found
        };
        for query in [
            "readme",
            "synergy readme",
            "readme !synergy",
            "plna",
            "home",
        ] {
            assert_eq!(found(query), paths(&index, query), "{query}");
        }
    }

    #[test]
    fn boosts_for_another_list_are_harmless() {
        let index = sample();
        let past_the_end = index.len() as EntryId;
        let boosts: Boosts = [(past_the_end, 48), (EntryId::MAX, 48), (0, 48)]
            .into_iter()
            .collect();
        let hits = boosted(&index, "readme", 10, &boosts);
        assert_eq!(hits, boosted(&index, "readme", 10, &Boosts::default()));
    }

    #[test]
    fn of_two_files_opened_the_one_opened_more_ranks_first() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let a = builder.push("a", root, true);
        let b = builder.push("b", root, true);
        let less = builder.push("plan.md", a, false);
        let more = builder.push("plan.md", b, false);
        let index = builder.finish();
        let boosts: Boosts = [(less, 12), (more, 24)].into_iter().collect();
        let hits = boosted(&index, "plan", 10, &boosts);
        assert_eq!(entries(&hits), [more, less]);
        assert_eq!(hits[0].score, hits[1].score + 12);
    }

    /// More names start with "pl" than the limit, so the search stops early, and still a
    /// deeper file holding it only inside its name climbs in by its boost.
    #[test]
    fn an_opened_file_with_the_word_inside_its_name_climbs_past_an_early_stop() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for i in 0..20 {
            builder.push(&format!("plan {i}.md"), root, false);
        }
        let deep = builder.push("deep", root, true);
        let inside = builder.push("xplx.md", deep, false);
        let index = builder.finish();
        let (apps, none) = (Apps::default(), Boosts::default());
        let (unboosted, stops) = with_and_without_shortcuts(&index, &apps, "pl", 5, &none);
        assert_eq!(stops, 1);
        assert!(!entries(&unboosted).contains(&inside));
        let boosts: Boosts = [(inside, 48)].into_iter().collect();
        let (hits, stops) = with_and_without_shortcuts(&index, &apps, "pl", 5, &boosts);
        assert_eq!(stops, 1);
        assert_eq!(hits[0].item, Item::Entry(inside), "{hits:?}");
        assert_eq!(hits[1..], unboosted[..4]);
    }

    /// Every hit scores exactly its score without boosts plus its own boost, and appears once.
    #[test]
    fn a_boost_adds_to_the_score_and_nothing_else() {
        let index = crate::stand_in(5_000);
        let mut rng = Rng(0xB005_7ED5);
        for pass in 0..10 {
            let boosts: Boosts = (0..rng.below(300))
                .map(|_| (rng.below(index.len()) as EntryId, 1 + rng.below(48) as u32))
                .collect();
            for query in ["budget", "plan notes", "re", "md !tax", "c"] {
                let all = index.len();
                let unboosted = boosted(&index, query, all, &Boosts::default());
                let base: std::collections::BTreeMap<Item, u32> =
                    unboosted.iter().map(|hit| (hit.item, hit.score)).collect();
                let hits = boosted(&index, query, all, &boosts);
                let items: std::collections::BTreeSet<Item> =
                    hits.iter().map(|hit| hit.item).collect();
                assert_eq!(items.len(), hits.len(), "pass {pass} {query:?}");
                assert_eq!(hits.len(), base.len(), "pass {pass} {query:?}");
                for hit in &hits {
                    let expected = base[&hit.item] + boosts.get(hit.item);
                    assert_eq!(hit.score, expected, "pass {pass} {query:?}");
                }
            }
        }
    }

    #[test]
    fn the_searcher_works_out_the_same_boosts() {
        let index = crate::stand_in(5_000);
        let mut frecency = Frecency::default();
        for entry in (1..index.len() as EntryId).step_by(97) {
            frecency.record(&index.path(entry), 1_790_000_000);
        }
        let searcher = Searcher::with_threads(2);
        let apps = Apps::default();
        let boosts = searcher.boosts(&frecency, &index, &apps, 1_790_000_000);
        assert!(boosts.len() > 40, "{}", boosts.len());
        assert_eq!(boosts, frecency.boosts(&index, &apps, 1_790_000_000));
    }

    /// The best `limit` hits with boosts are the first `limit` of all hits with boosts, so
    /// checking only the boosted entries past the limit loses nothing.
    #[test]
    fn boosted_hits_are_the_best_whatever_the_limit() {
        let index = crate::stand_in(5_000);
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % below as u64) as usize
        };
        for pass in 0..20 {
            let boosts: Boosts = (0..next(300))
                .map(|_| (next(index.len()) as EntryId, 1 + next(48) as u32))
                .collect();
            for query in ["budget", "plan notes", "re", "md !tax", "readme"] {
                let all = boosted(&index, query, index.len(), &boosts);
                for limit in [1, 5, 50] {
                    let few = boosted(&index, query, limit, &boosts);
                    let best = &all[..limit.min(all.len())];
                    assert_eq!(few, best, "pass {pass} {query:?} limit {limit}");
                }
            }
        }
    }

    #[test]
    fn a_boosted_file_beats_an_equal_match_with_apps_found_too() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let a = builder.push("a", root, true);
        let b = builder.push("b", root, true);
        let first = builder.push("plan.md", a, false);
        let opened = builder.push("plan.md", b, false);
        let index = builder.finish();
        let apps = applications(&["Plan", "Planner"]);
        let none = Boosts::default();
        let unboosted = boosted_with(&index, &apps, "plan", 10, &none);
        assert_eq!(entries(&unboosted), [first, opened]);
        let boosts: Boosts = [(opened, 1)].into_iter().collect();
        let hits = boosted_with(&index, &apps, "plan", 10, &boosts);
        assert_eq!(entries(&hits), [opened, first]);
        let apps_found = |hits: &[Hit]| -> Vec<Hit> {
            let apps = hits.iter().filter(|hit| matches!(hit.item, Item::App(_)));
            apps.copied().collect()
        };
        assert_eq!(apps_found(&hits), apps_found(&unboosted));
        assert_eq!(apps_found(&hits).len(), 2);
    }

    /// Opening an app counts by its bundle's path, so the app row climbs, and the bundle the
    /// index holds too stays hidden behind it.
    #[test]
    fn an_opened_app_climbs_past_a_better_named_one() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let folder = builder.push("Applications", root, true);
        builder.push("Gmail.app", folder, true);
        builder.push("Gmail Helper.app", folder, true);
        builder.push("Gmail notes.txt", folder, false);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &["Gmail", "Gmail Helper"]);
        let path = |hit: &Hit| match hit.item {
            Item::Entry(entry) => index.path(entry),
            Item::App(app) => apps.path(app),
        };
        let none = Boosts::default();
        let unboosted = boosted_with(&index, &apps, "gmail", 10, &none);
        assert_eq!(unboosted[0].item, Item::App(0));
        assert_eq!(unboosted[1].item, Item::App(1));

        let mut frecency = Frecency::default();
        let helper = Path::new("/home/Applications/Gmail Helper.app");
        frecency.record(helper, 1_790_000_000);
        frecency.record(helper, 1_790_000_000);
        let boosts = frecency.boosts(&index, &apps, 1_790_000_000);
        let hits = boosted_with(&index, &apps, "gmail", 10, &boosts);
        assert_eq!(hits[0].item, Item::App(1), "{hits:?}");
        assert_eq!(hits[0].score, unboosted[1].score + boosts.get(Item::App(1)));
        let shown: Vec<std::path::PathBuf> = hits.iter().map(path).collect();
        assert_eq!(shown.iter().filter(|shown| **shown == helper).count(), 1);
        assert_eq!(hits.len(), unboosted.len());
    }

    /// Bundles in the index widen the early stop, and a file climbing in by its boost takes a
    /// place in that wider list, so every limit still keeps the head of the full list.
    #[test]
    fn the_early_stop_holds_with_apps_and_climbing_files() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for i in 0..20 {
            builder.push(&format!("plan {i}.md"), root, false);
        }
        let folder = builder.push("Applications", root, true);
        builder.push("Plato.app", folder, true);
        builder.push("Pluto.app", folder, true);
        let deep = builder.push("deep", root, true);
        let inside = builder.push("xplx.md", deep, false);
        let index = builder.finish();
        let apps = Apps::from_names(Path::new("/home/Applications"), &["Plato", "Pluto"]);
        let none = Boosts::default();
        let (unboosted, stops) = with_and_without_shortcuts(&index, &apps, "pl", 5, &none);
        assert_eq!(stops, 1);
        let (plato, pluto) = (Item::App(0), Item::App(1));
        let items = |hits: &[Hit]| -> Vec<Item> { hits.iter().map(|hit| hit.item).collect() };
        assert!(!entries(&unboosted).contains(&inside));
        assert_eq!(items(&unboosted)[..2], [plato, pluto]);
        let boosts = [(inside, 48)]
            .into_iter()
            .collect::<Boosts>()
            .with_apps([(1, 30)]);
        let (hits, stops) = with_and_without_shortcuts(&index, &apps, "pl", 5, &boosts);
        assert_eq!(stops, 1);
        assert!(entries(&hits).contains(&inside), "{hits:?}");
        assert_eq!(items(&hits)[..3], [Item::Entry(inside), pluto, plato]);
        let all = boosted_with(&index, &apps, "pl", 1_000, &boosts);
        for limit in 1..=all.len() {
            let (few, _) = with_and_without_shortcuts(&index, &apps, "pl", limit, &boosts);
            assert_eq!(few, all[..limit], "limit {limit}");
        }
    }

    #[test]
    fn searches_in_a_row_do_not_leak_into_each_other() {
        let index = sample();
        let apps = Apps::from_names(Path::new("/Applications"), &["Synergy", "Readme"]);
        let searcher = Searcher::with_threads(2);
        let search = |searcher: &Searcher, query| {
            searcher
                .search(&index, &apps, query, searcher.ticket(), 10)
                .unwrap()
        };
        search(&searcher, "synergy readme");
        assert_eq!(search(&searcher, "plan").len(), 1);
        search(&searcher, "synergy readme");
        let after = search(&searcher, "synergy");
        assert_eq!(after, search(&Searcher::with_threads(2), "synergy"));
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

    /// Issue #71: "Lark" showed LarkSuite first only because a capital typed leaves out every
    /// lower-case name; "lark" let a folder named lark win.
    #[test]
    fn the_first_word_of_an_app_finds_it_first_in_any_case() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("lark", root, true);
        builder.push("lark_report.py", root, false);
        builder.push("schema-lark", root, true);
        builder.push("iCloud~com~larksuite~lark", root, true);
        builder.push("LarkReport.pdf", root, false);
        // Saved editor history is full of names like this; its capitals mark no words.
        builder.push("sLaR.js", root, false);
        let index = builder.finish();
        let apps = applications(&["LarkSuite", "Safari"]);
        for query in ["lark", "Lark"] {
            let found = ranked(&index, &apps, query);
            assert_eq!(found[0], "/Applications/LarkSuite.app", "{query} {found:?}");
        }
        let found = ranked(&index, &apps, "lar");
        assert_ne!(found[0], "/home/sLaR.js", "{found:?}");
    }

    /// A developer typing "go" wants ~/go, their Go workspace, not GoLand.
    #[test]
    fn a_short_query_keeps_a_folder_of_that_name_above_an_app_starting_with_it() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        builder.push("go", root, true);
        let index = builder.finish();
        let apps = applications(&["GoLand", "WeChat"]);
        let found = ranked(&index, &apps, "go");
        assert_eq!(found[0], "/home/go", "{found:?}");
    }

    /// Someone who starts Xcode from the Dock and once opened Xcode-beta through Argus still
    /// wants Xcode for "xcode", so it must lead on points, not only on its shorter name.
    #[test]
    fn an_app_named_exactly_what_was_typed_outscores_a_longer_one() {
        let index = IndexBuilder::new().finish();
        let apps = applications(&["Xcode-beta", "Xcode", "Notion Calendar", "Notion"]);
        let searcher = Searcher::with_threads(2);
        for (query, exact) in [("xcode", "Xcode"), ("notion", "Notion")] {
            let hits = searcher
                .search(&index, &apps, query, searcher.ticket(), 10)
                .unwrap();
            assert_eq!(hits.len(), 2, "{query}");
            assert!(matches!(hits[0].item, Item::App(app) if apps.name(app) == exact));
            assert!(hits[0].score > hits[1].score, "{query}");
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
    fn an_app_holds_a_word_typed_on_its_word_starts() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for name in [
            "SATU.xcodeproj",
            "visionmodel",
            "synergy-core.code-workspace",
            "design.pdf",
        ] {
            builder.push(name, root, false);
        }
        let index = builder.finish();
        let apps = applications(&["Visual Studio Code"]);
        // V·S·Code is how people type it, so files one typo from "vscode" stay out.
        assert_eq!(
            ranked(&index, &apps, "vscode"),
            ["/Applications/Visual Studio Code.app"]
        );
        assert_eq!(ranked(&index, &apps, "desing"), ["/home/design.pdf"]);
    }

    #[test]
    fn an_app_holds_a_word_still_being_typed_on_its_word_starts() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for name in ["SATU.xcodeproj", "Discord.app", "visionmodel"] {
            builder.push(name, root, false);
        }
        let index = builder.finish();
        let apps = applications(&["Visual Studio Code"]);
        // Each keystroke on the way to "vscode" keeps the list clean, not just the last.
        for query in ["vsco", "vscod"] {
            assert_eq!(
                ranked(&index, &apps, query),
                ["/Applications/Visual Studio Code.app"],
                "{query}"
            );
        }
    }

    #[test]
    fn a_plural_is_not_an_app_typed_on_its_word_starts() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let configurations = builder.push("configurations", root, true);
        builder.push("colors.json", configurations, false);
        builder.push("Assignment Embedded Systems.docx", root, false);
        builder.push("curl", root, false);
        let index = builder.finish();
        let apps = applications(&[
            "ColorSync Utility",
            "System Settings",
            "Claude Code URL Handler",
        ]);
        // "colors" is Color + S of Sync, but someone typing it wants the file named colors.
        let tops: Vec<String> = ["colors", "systems", "curl"]
            .iter()
            .map(|query| ranked(&index, &apps, query).swap_remove(0))
            .collect();
        assert_eq!(
            tops,
            [
                "/home/configurations/colors.json",
                "/home/Assignment Embedded Systems.docx",
                "/home/curl",
            ]
        );
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

    use std::collections::BTreeSet;

    /// Every file name below sits in one folder, and each query lists what it must find and
    /// what it must leave out.
    #[test]
    fn a_word_never_takes_letters_from_inside_an_extension() {
        let names = [
            "credit-mutation.tsx",
            "credit-mutation.xlsx",
            "cmux",
            "cmux.app",
            "NIB.pdf",
            "report.pdf",
            "main.rs",
            "com.apple.Safari",
            "com.apple.Safari.plist",
            "v1.2.3",
            "archive.tar.gz",
            "jquery-3.6.0.min.js",
            "Shop.xcworkspace",
            ".zshrc",
            ".env.local",
            "Makefile",
            "notes.",
            "résumé.pdf",
            "photo.JPG",
            "budget 2024.numbers",
            "Mr. Robot",
            "St. Louis trip",
            "node-v20.11.1-darwin-arm64",
            "Info.plist",
            "com.microsoft.VSCode",
            "design.pdf",
            "aB.CCbC",
            "next.config.mjs",
            "Python-3.12.1.tgz",
            "Xcode.app",
            "Lease Agreement.pdf",
        ];
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        for name in names {
            builder.push(name, root, false);
        }
        let index = builder.finish();
        let found = |query: &str| -> Vec<String> {
            paths(&index, query)
                .into_iter()
                .map(|path| path.trim_start_matches("/home/").to_owned())
                .collect()
        };
        let cases: &[(&str, &[&str], &[&str])] = &[
            // The x of "cmux" is only in the middle of ".tsx".
            (
                "cmux",
                &["cmux", "cmux.app", "credit-mutation.xlsx"],
                &["credit-mutation.tsx"],
            ),
            ("cmx", &["cmux", "cmux.app"], &["credit-mutation.tsx"]),
            ("crdtsx", &["credit-mutation.tsx"], &[]),
            ("crs", &[], &["credit-mutation.tsx"]),
            // A word wholly in the extension, or wholly before it, still finds the file.
            ("pdf", &["NIB.pdf", "report.pdf", "résumé.pdf"], &[]),
            ("tsx", &["credit-mutation.tsx"], &[]),
            (
                "mutation",
                &["credit-mutation.tsx", "credit-mutation.xlsx"],
                &[],
            ),
            (
                "credmut",
                &["credit-mutation.tsx", "credit-mutation.xlsx"],
                &[],
            ),
            ("nib", &["NIB.pdf"], &[]),
            ("jpg", &["photo.JPG"], &[]),
            ("numbers", &["budget 2024.numbers"], &[]),
            ("workspace", &["Shop.xcworkspace"], &[]),
            // Typing on into the first letters of the extension still finds it.
            ("reportpdf", &["report.pdf"], &[]),
            ("reportp", &["report.pdf"], &[]),
            ("mainrs", &["main.rs"], &[]),
            (
                "applesafari",
                &["com.apple.Safari", "com.apple.Safari.plist"],
                &[],
            ),
            ("safariplist", &["com.apple.Safari.plist"], &[]),
            ("v123", &["v1.2.3"], &[]),
            ("targz", &["archive.tar.gz"], &[]),
            ("minjs", &["jquery-3.6.0.min.js"], &[]),
            ("envlocal", &[".env.local"], &[]),
            ("photojpg", &["photo.JPG"], &[]),
            // Or from its second letter, the first left out, two letters or more.
            ("nextconfigjs", &["next.config.mjs"], &[]),
            ("pythongz", &["Python-3.12.1.tgz"], &[]),
            ("rdf", &["report.pdf"], &[]),
            // But not one letter from further in: the f of "rpf" is the last of ".pdf".
            ("rpf", &[], &["report.pdf"]),
            ("mns", &[], &["main.rs"]),
            ("nbf", &[], &["NIB.pdf"]),
            // From four letters on, a word no name holds may take a typo: "mains" is "main"
            // with an s missing, the way "desing" finds design.pdf, and "reportdf" is
            // "reportpdf" with the p missing.
            ("mains", &["main.rs"], &[]),
            ("reportdf", &["report.pdf"], &[]),
            // A dot typed, or a literal word, goes wherever it says.
            ("mutation.tsx", &["credit-mutation.tsx"], &[]),
            ("n.tsx", &["credit-mutation.tsx"], &[]),
            (
                "cmu.x",
                &["credit-mutation.tsx", "credit-mutation.xlsx"],
                &[],
            ),
            ("tsx$", &["credit-mutation.tsx"], &[]),
            ("'n.tsx", &["credit-mutation.tsx"], &[]),
            (".zsh", &[".zshrc"], &[]),
            ("zshrc", &[".zshrc"], &[]),
            // A name with no extension, or a lone dot at its end, is matched as before.
            ("mkfl", &["Makefile"], &[]),
            ("notes", &["notes."], &[]),
            // Text after an abbreviation's dot, with a space or dash in it, is no extension.
            ("mrrobot", &["Mr. Robot"], &[]),
            ("stlouis", &["St. Louis trip"], &[]),
            ("nodearm64", &["node-v20.11.1-darwin-arm64"], &[]),
            // Three or more letters in a row may come from anywhere in the extension.
            ("shopworkspace", &["Shop.xcworkspace"], &[]),
            ("infolist", &["Info.plist"], &[]),
            ("microsoftcode", &["com.microsoft.VSCode"], &[]),
            // A word no name holds may spend its one typo before the dot or after it.
            ("designpfd", &["design.pdf"], &[]),
            ("shopworkspcae", &["Shop.xcworkspace"], &[]),
            ("shopwrokspace", &["Shop.xcworkspace"], &[]),
            ("infolsit", &["Info.plist"], &[]),
            ("microsoftcdoe", &["com.microsoft.VSCode"], &[]),
            ("reportodf", &["report.pdf"], &[]),
            ("leaseodf", &["Lease Agreement.pdf"], &[]),
            ("leasedf", &["Lease Agreement.pdf"], &[]),
            ("mainfs", &["main.rs"], &[]),
            ("résuméodf", &["résumé.pdf"], &[]),
            // The extension's first two letters swapped is one typo too.
            ("reportdpf", &["report.pdf"], &[]),
            ("leasedpf", &["Lease Agreement.pdf"], &[]),
            ("nextconfigjms", &["next.config.mjs"], &[]),
            ("photopjg", &["photo.JPG"], &[]),
            ("mainsr", &["main.rs"], &[]),
            // But the typo does not open the middle of the extension: "mainsx" would need the
            // s from inside ".rs" as well as a typo for the x.
            ("mainsx", &[], &["main.rs"]),
            // A capital typed minds case in every piece: the b of "abC" is only deep inside
            // "CCbC", while "abc" fits "aB" and the C opening it.
            ("abC", &[], &["aB.CCbC"]),
            ("abc", &["aB.CCbC"], &[]),
            // Each word of several may sit on its own side of the dot.
            ("nib pdf", &["NIB.pdf"], &[]),
            ("credit tsx", &["credit-mutation.tsx"], &[]),
            (
                "cmux !tsx",
                &["cmux", "cmux.app", "credit-mutation.xlsx"],
                &[],
            ),
        ];
        for &(query, kept, dropped) in cases {
            let found = found(query);
            for name in kept {
                assert!(
                    found.iter().any(|f| f == name),
                    "{query:?} lost {name:?}: {found:?}"
                );
            }
            for name in dropped {
                assert!(!found.iter().any(|f| f == name), "{query:?} found {name:?}");
            }
        }
    }

    #[test]
    fn the_cmux_app_comes_without_credit_mutation() {
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let src = builder.push("src", root, true);
        builder.push("credit-mutation.tsx", src, false);
        let index = builder.finish();
        let apps = applications(&["cmux"]);
        assert_eq!(ranked(&index, &apps, "cmux"), ["/Applications/cmux.app"]);
        // With no name holding "cmux" it may be a typo, and "cmu" fits before the dot.
        assert_eq!(
            ranked(&index, &Apps::default(), "cmux"),
            ["/home/src/credit-mutation.tsx"]
        );
    }

    /// ASCII with no typo: frizbee's fuzzy match is then "the word's letters appear in order",
    /// minding case once the word has a capital, so the rule can be checked against every way
    /// of splitting the word. Written apart from `extension` on purpose.
    fn fits_by_hand(name: &str, word: &str) -> bool {
        let fold = |text: &str| {
            if word.bytes().any(|b| b.is_ascii_uppercase()) {
                text.to_owned()
            } else {
                text.to_ascii_lowercase()
            }
        };
        let in_order = |word: &str, text: &str| {
            let text = fold(text);
            let mut text = text.chars();
            word.chars().all(|c| text.any(|t| t == c))
        };
        let dot = name.rfind('.').filter(|&dot| {
            let tail = &name[dot + 1..];
            dot > 0 && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let Some(dot) = dot.filter(|_| !word.contains('.')) else {
            return in_order(word, name);
        };
        let (stem, tail) = (&name[..dot], fold(&name[dot + 1..]));
        in_order(word, stem)
            || in_order(word, &tail)
            || (1..word.len()).any(|cut| {
                let end = &word[cut..];
                let fits = tail.starts_with(end)
                    || (end.len() >= 2 && tail[1..].starts_with(end))
                    || (end.len() >= 3 && tail.contains(end));
                fits && in_order(&word[..cut], stem)
            })
    }

    #[test]
    fn the_extension_rule_matches_every_way_of_splitting_the_word() {
        let mut rng = Rng(0xd07);
        let parts = [
            "a", "b", "c", "A", "B", ".", ".", "-", " ", "ab", "abc", "é", "1",
        ];
        let typed = ["a", "b", "c", "a", "b", "c", ".", "A", "1"];
        let config = matcher_config();
        let (mut kept, mut dropped) = (0, 0);
        for _ in 0..6_000 {
            let needle = rng.text(&typed, 6);
            let Some(word) = Pattern::parse_query(&needle).pop() else {
                continue;
            };
            let word = positive(word).max_typos(Some(0));
            let names: Vec<String> = (0..40).map(|_| rng.name(&parts, 10)).collect();
            let mut matcher = Matcher::new(word.clone(), &config);
            let mut sides = Sides::new(&word, 0);
            for found in Matcher::new(word.clone(), &config).match_list(&names) {
                let name = &names[found.index as usize];
                let ours = !splits_at_dot(&word)
                    || beside_extension(name, &word, &mut matcher, &mut sides);
                assert_eq!(
                    ours,
                    fits_by_hand(name, &word.needle),
                    "{needle:?} in {name:?}"
                );
                if ours {
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        assert!(
            kept > 5_000 && dropped > 300,
            "{kept} kept, {dropped} dropped"
        );
    }

    /// `fits_by_hand` for a lower-case ASCII word allowed one typo, which to frizbee is one
    /// letter of the word that the name may lack.
    fn fits_by_hand_with_a_typo(name: &str, word: &str) -> bool {
        let in_order = |word: &str, text: &str| {
            let mut text = text.bytes().map(|b| b.to_ascii_lowercase());
            word.bytes().all(|w| text.any(|t| t == w))
        };
        let near = |word: &str, text: &str| {
            in_order(word, text)
                || (0..word.len())
                    .any(|i| in_order(&format!("{}{}", &word[..i], &word[i + 1..]), text))
        };
        let dot = name.rfind('.').filter(|&dot| {
            let tail = &name[dot + 1..];
            dot > 0 && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let Some(dot) = dot else {
            return true;
        };
        let (stem, tail) = (&name[..dot], name[dot + 1..].to_ascii_lowercase());
        let exact = |end: &str| {
            tail.starts_with(end)
                || (end.len() >= 2 && tail[1..].starts_with(end))
                || (end.len() >= 3 && tail.contains(end))
        };
        near(word, stem)
            || near(word, &tail)
            || (1..word.len()).any(|cut| {
                let (head, end) = word.split_at(cut);
                (near(head, stem) && exact(end))
                    || (in_order(head, stem)
                        && ((near(end, &tail) && (end.len() >= 4 || tail.starts_with(&end[..1])))
                            || tail[1..].starts_with(&end[1..])
                            || (end.len() >= 2
                                && tail.starts_with(&format!(
                                    "{}{}{}",
                                    &end[1..2],
                                    &end[..1],
                                    &end[2..]
                                )))))
            })
    }

    #[test]
    fn the_extension_rule_with_a_typo_matches_every_way_of_spending_it() {
        let mut rng = Rng(0x7790);
        let parts = [
            "a", "b", "c", "d", ".", ".", "-", " ", "ab", "abc", "cd", "1",
        ];
        let typed = ["a", "b", "c", "d", "a", "b"];
        let config = matcher_config();
        let (mut kept, mut dropped) = (0, 0);
        for _ in 0..6_000 {
            let needle = rng.text(&typed, 7);
            let word = Pattern::parse_query(&needle).remove(0).max_typos(Some(1));
            let names: Vec<String> = (0..40).map(|_| rng.name(&parts, 12)).collect();
            let mut matcher = Matcher::new(word.clone(), &config);
            let mut sides = Sides::new(&word, 1);
            for found in Matcher::new(word.clone(), &config).match_list(&names) {
                let name = &names[found.index as usize];
                let ours = beside_extension(name, &word, &mut matcher, &mut sides);
                assert_eq!(
                    ours,
                    fits_by_hand_with_a_typo(name, &needle),
                    "{needle:?} in {name:?}"
                );
                if ours {
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        assert!(
            kept > 5_000 && dropped > 500,
            "{kept} kept, {dropped} dropped"
        );
    }

    #[test]
    fn letters_in_order_is_a_frizbee_match() {
        let mut rng = Rng(0x0dd);
        let parts = ["a", "b", "A", "B", "1", ".", "-", "_", " ", "(", "~"];
        let typed = ["a", "b", "A", "B", "1", "-", "_", "~"];
        let config = matcher_config();
        let mut checked = 0;
        for _ in 0..3_000 {
            let needle = rng.text(&typed, 6);
            let word = Pattern::parse_query(&needle).remove(0).max_typos(Some(0));
            let respect = needle.bytes().any(|b| b.is_ascii_uppercase());
            let names: Vec<String> = (0..40).map(|_| rng.name(&parts, 12)).collect();
            let mut frizbee = vec![false; names.len()];
            for found in Matcher::new(word.clone(), &config).match_list(&names) {
                frizbee[found.index as usize] = true;
            }
            for (name, frizbee) in names.iter().zip(frizbee) {
                if in_order(&word.needle, name, respect) {
                    checked += 1;
                    assert!(frizbee, "{needle:?} in {name:?}");
                }
            }
        }
        assert!(checked > 10_000, "{checked}");
    }

    /// The quick check by letters gives frizbee's answer for every ASCII word with no typo.
    #[test]
    fn the_letter_check_agrees_with_frizbee_on_every_part() {
        let mut rng = Rng(0xa5c1);
        let parts = [
            "a", "b", "c", "A", "B", "1", ".", ".", "-", "_", " ", "abc", "ABC",
        ];
        let typed = ["a", "b", "c", "A", "B", "C", "1", "_"];
        let config = matcher_config();
        let (mut kept, mut dropped) = (0, 0);
        for _ in 0..4_000 {
            let needle = rng.text(&typed, 7);
            let word = Pattern::parse_query(&needle).remove(0).max_typos(Some(0));
            let respect = needle.bytes().any(|b| b.is_ascii_uppercase());
            let mut matcher = Matcher::new(word.clone(), &config);
            let mut sides = Sides::new(&word, 0);
            for _ in 0..30 {
                let name = rng.name(&parts, 14);
                let Some((stem, tail)) = extension(&name) else {
                    continue;
                };
                let ours = fits_by_letters(stem, tail, &needle, respect);
                let frizbee = fits_by_frizbee(stem, tail, &word, &mut matcher, &mut sides);
                assert_eq!(ours, frizbee, "{needle:?} in {name:?}");
                if ours {
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        assert!(
            kept > 2_000 && dropped > 2_000,
            "{kept} kept, {dropped} dropped"
        );
    }

    /// A word that fits before the dot, or in the extension alone, is never dropped, however it
    /// is cased, whatever typo it may take, with letters from beyond ASCII too.
    #[test]
    fn the_extension_rule_never_drops_a_word_one_side_holds() {
        let mut rng = Rng(0xe47);
        let parts = [
            "a", "b", "x", "A", "X", ".", "-", "_", " ", "é", "É", "日", "1",
        ];
        let typed = ["a", "b", "x", "A", "X", "1", "é", "É"];
        let config = matcher_config();
        let mut checked = 0;
        for _ in 0..3_000 {
            let needle = rng.text(&typed, 6);
            let typos = rng.below(2) as u16;
            let word = Pattern::parse_query(&needle)
                .remove(0)
                .max_typos(Some(typos));
            let names: Vec<String> = (0..40).map(|_| rng.name(&parts, 12)).collect();
            let mut matcher = Matcher::new(word.clone(), &config);
            let mut sides = Sides::new(&word, typos);
            let mut one_side = Matcher::new(word.clone(), &config);
            for found in Matcher::new(word.clone(), &config).match_list(&names) {
                let name = &names[found.index as usize];
                let holds = extension(name).is_none_or(|(stem, tail)| {
                    one_side.match_one(stem, 0).is_some() || one_side.match_one(tail, 0).is_some()
                });
                if holds {
                    checked += 1;
                    assert!(
                        beside_extension(name, &word, &mut matcher, &mut sides),
                        "{needle:?} with {typos} typos in {name:?}"
                    );
                }
            }
        }
        assert!(checked > 5_000, "{checked}");
    }

    /// The whole search, every shortcut and pass included, finds exactly the names frizbee
    /// matches and the rule keeps, for words too short to take a typo.
    #[test]
    fn a_search_finds_exactly_the_names_the_extension_rule_keeps() {
        let mut rng = Rng(0x5eac);
        let parts = ["a", "b", "c", "x", ".", ".", "-", "ab", "cx", "tsx"];
        let mut builder = IndexBuilder::new();
        let root = builder.push("/home", NO_PARENT, true);
        let mut names = Vec::new();
        for i in 0..600 {
            let name = format!(
                "{}{}",
                rng.name(&parts, 8),
                ["", ".tsx", ".ab", ".x"][i % 4]
            );
            builder.push(&name, root, false);
            names.push(name);
        }
        let index = builder.finish();
        let config = matcher_config();
        let never = &|| false;
        let mut checked = 0;
        for _ in 0..200 {
            let query = rng.text(&["a", "b", "c", "x", "t"], 3);
            let word = Pattern::parse_query(&query).remove(0);
            let texts: Vec<&str> = names
                .iter()
                .map(|name| name.strip_prefix('.').unwrap_or(name))
                .collect();
            let expected: BTreeSet<&str> = Matcher::new(positive(word), &config)
                .match_list(&texts)
                .iter()
                .map(|found| found.index as usize)
                .filter(|&n| fits_by_hand(texts[n], &query))
                .map(|n| names[n].as_str())
                .collect();
            for no_shortcuts in [false, true] {
                let mut scratch = Scratch {
                    no_shortcuts,
                    ..Scratch::default()
                };
                let hits = run(
                    &index,
                    &Apps::default(),
                    &query,
                    index.len(),
                    None,
                    &Boosts::default(),
                    &mut scratch,
                    never,
                )
                .unwrap();
                let found: BTreeSet<&str> = hits
                    .iter()
                    .map(|hit| index.name(hit.entry().unwrap()))
                    .collect();
                assert_eq!(found, expected, "{query:?}");
            }
            checked += expected.len();
        }
        assert!(checked > 2_000, "{checked}");
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
