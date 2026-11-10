//! The typed text found in lines as names find it: accents off on both sides, so "cafe" and
//! "café" find each other, and any case when typed all in lower case.

use grep_matcher::{LineTerminator, Match, Matcher, NoCaptures, NoError};
use std::iter;
use std::ops::{Range, RangeInclusive};
use std::str::Chars;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Literal {
    /// The folded text, lower-cased with a final ς as σ when typed all in lower case.
    text: String,
    ascii: bool,
    any_case: bool,
    /// Each letter of the text, any of which can anchor a search.
    ways: Vec<Ways>,
    /// Two for each of `ways`: the needles guessed rarest, and their first bytes alone.
    anchors: Vec<Anchor>,
    /// The anchor the next block of lines starts with, as files read together are mostly in
    /// one language: an index into `anchors`, then into `fresh`.
    chosen: AtomicUsize,
    /// Needles blocks chose afresh from their own bytes, and how many slots are taken.
    fresh: [OnceLock<Anchor>; RENEWALS],
    renewals: AtomicUsize,
    /// Choosing afresh costs about as much as reading a file, so even failed tries are few.
    attempts: AtomicUsize,
}

/// A letter of the text and each way a line can write it.
struct Ways {
    /// How many bytes of the text come before it.
    before: usize,
    /// Accents and all, sorted.
    spellings: Vec<char>,
    /// Letters that hold it after the letters just before it in the text, like the þ that
    /// holds the h of "the", and how many bytes of the text they hold before it.
    inside: Vec<(char, usize)>,
}

/// The letter of the text looked for first, and the bytes it is found by.
struct Anchor {
    /// Which of the text's `ways`.
    way: usize,
    /// Sorted, three to a pass: an ASCII spelling, or the first, second or last byte of
    /// others, whichever are rarest.
    needles: Vec<u8>,
    /// The bytes just before a needle that is the second or last byte of a spelling.
    follows: [bool; 256],
}

/// What one more pass over a line costs in `commonness`: as much as a needle found every
/// 250 bytes or so.
const PASS: usize = 40;

/// The most passes the spellings of a letter take: a test checks every letter.
const MOST_PASSES: usize = 8;

/// How far a search reads before checking that its needles are rare in the line.
const SAMPLE: usize = 4096;

/// How many times a search chooses needles afresh, each about as slow as reading a file.
const RENEWALS: usize = 8;

/// What stopping on a needle costs in ten thousand bytes before a line is crowded, at least:
/// an ASCII letter in every 128 bytes, as `stop` counts.
const CROWD: usize = 10_000 / 128;

/// What stopping on `byte` costs: an ASCII letter is quickly compared, and one beyond ASCII
/// is read whole and folded, about four times as slow.
fn stop(byte: u8) -> usize {
    if byte.is_ascii() { 1 } else { 4 }
}

impl Anchor {
    /// The cheapest needles for `ways` where bytes and passes cost what `cost` and `pass` say.
    fn cover(way: usize, ways: &Ways, cost: impl Fn(u8) -> usize, pass: usize) -> Self {
        let (needles, follows) = needles(&ways.encoded(), cost, pass);
        Self {
            way,
            needles,
            follows,
        }
    }

    /// The first bytes alone, which find lines in another alphabet in fewer passes, like the
    /// C5 and E1 of ź and ẑ in Cyrillic.
    fn firsts(way: usize, ways: &Ways) -> Self {
        Self {
            way,
            needles: first_bytes(&ways.encoded()),
            follows: [false; 256],
        }
    }

    /// What finding the needles costs where a byte shows up `count` times, and one more pass
    /// over the line as much as `pass` of them.
    fn cost(&self, count: impl Fn(u8) -> usize, pass: usize) -> usize {
        let found: usize = self.needles.iter().map(|&needle| count(needle)).sum();
        found + pass * self.needles.len().div_ceil(3)
    }

    /// Whether a needle found at `at` can be part of a spelling: a byte inside a letter only
    /// after the byte a spelling has before it.
    #[inline]
    fn fits(&self, haystack: &[u8], at: usize) -> bool {
        !crate::is_inside_letter(haystack[at])
            || (at > 0 && self.follows[usize::from(haystack[at - 1])])
    }
}

/// The bytes that find spellings `encoded` in UTF-8 and sorted, cheapest where a byte costs
/// what `cost` says and a pass `pass`, and the bytes just before those inside a spelling.
fn needles(encoded: &[Vec<u8>], cost: impl Fn(u8) -> usize, pass: usize) -> (Vec<u8>, [bool; 256]) {
    // Each spelling is found by its first byte, its second when it has three, or its last.
    let places = |s: &[u8]| {
        let places = [Some(0), (s.len() > 2).then_some(1), Some(s.len() - 1)];
        places.into_iter().flatten()
    };
    // The spellings each byte can find.
    let mut holders = vec![Vec::new(); 256];
    for (spelling, s) in encoded.iter().enumerate() {
        for at in places(s) {
            let holders: &mut Vec<usize> = &mut holders[usize::from(s[at])];
            if holders.last() != Some(&spelling) {
                holders.push(spelling);
            }
        }
    }
    // Each needle takes a third of a pass. Each round takes the byte that costs the least for
    // each spelling it finds, as the first byte shared by Cyrillic letters finds them all.
    let each = |byte: u8| cost(byte) + pass / 3;
    let (mut found, mut needles) = (vec![false; encoded.len()], Vec::new());
    while found.contains(&false) {
        let finds = |byte: u8| {
            (holders[usize::from(byte)].iter())
                .filter(|&&s| !found[s])
                .count()
        };
        let (byte, _) = (0..=u8::MAX)
            .map(|byte| (byte, finds(byte)))
            .filter(|&(_, finds)| finds > 0)
            .min_by(|&(a, finds_a), &(b, finds_b)| (each(a) * finds_b).cmp(&(each(b) * finds_a)))
            .expect("every spelling holds bytes");
        needles.push(byte);
        holders[usize::from(byte)]
            .iter()
            .for_each(|&s| found[s] = true);
    }
    needles.sort_unstable();
    needles.dedup();
    // So many that they would take too many passes: the first bytes, fewer.
    if needles.len() > 3 * MOST_PASSES {
        needles = first_bytes(encoded);
    }
    let mut follows = [false; 256];
    for s in encoded {
        for at in places(s).filter(|&at| at > 0 && needles.contains(&s[at])) {
            follows[usize::from(s[at - 1])] = true;
        }
    }
    (needles, follows)
}

fn first_bytes(encoded: &[Vec<u8>]) -> Vec<u8> {
    let mut firsts: Vec<u8> = encoded.iter().map(|s| s[0]).collect();
    firsts.sort_unstable();
    firsts.dedup();
    firsts
}

impl Ways {
    /// Each spelling and letter that holds this one, in UTF-8.
    fn encoded(&self) -> Vec<Vec<u8>> {
        let inside = self.inside.iter().map(|&(letter, _)| letter);
        (self.spellings.iter().copied().chain(inside))
            .map(|letter| letter.to_string().into())
            .collect()
    }
}

impl Literal {
    /// `None` for empty text, text that runs over two lines, or accents alone, which fold to
    /// nothing as in names.
    pub(crate) fn new(text: &str) -> Option<Self> {
        if !crate::can_search(text) {
            return None;
        }
        let mut folded = String::with_capacity(text.len());
        for letter in text.chars() {
            if !folds(letter, |part| folded.push(part)) {
                folded.push(letter);
            }
        }
        let any_case = folded.chars().any(has_case) && !folded.chars().any(char::is_uppercase);
        let text: String = if any_case {
            folded.to_lowercase().chars().map(sigma).collect()
        } else {
            folded
        };
        // Lines holding the text are mostly in its alphabet, like Cyrillic letters, which all
        // start with one of two bytes.
        let mut own = [false; 256];
        for letter in text.chars().filter(|letter| !letter.is_ascii()) {
            own[usize::from(letter.encode_utf8(&mut [0; 4]).as_bytes()[0])] = true;
        }
        let later = &folded_letters().later;
        let ways: Vec<Ways> = (text.char_indices())
            .map(|(before, letter)| {
                let start = later.partition_point(|(part, ..)| *part < letter);
                let mut inside: Vec<(char, usize)> = (later[start..].iter())
                    .take_while(|(part, ..)| *part == letter)
                    .filter(|(_, earlier, _)| text[..before].ends_with(earlier.as_str()))
                    .map(|(_, earlier, holder)| (*holder, earlier.len()))
                    .collect();
                inside.sort_unstable();
                inside.dedup();
                Ways {
                    before,
                    spellings: ways_to_write(letter, any_case),
                    inside,
                }
            })
            .collect();
        let guess = |byte: u8| {
            if own[usize::from(byte)] {
                1000
            } else {
                commonness(byte)
            }
        };
        let anchors: Vec<Anchor> = (ways.iter().enumerate())
            .flat_map(|(way, ways)| {
                [
                    Anchor::cover(way, ways, guess, PASS),
                    Anchor::firsts(way, ways),
                ]
            })
            .collect();
        let mut literal = Self {
            ways,
            anchors,
            chosen: AtomicUsize::new(0),
            fresh: [const { OnceLock::new() }; RENEWALS],
            renewals: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
            ascii: text.is_ascii(),
            text,
            any_case,
        };
        *literal.chosen.get_mut() = literal.cheapest(guess, PASS);
        Some(literal)
    }

    fn anchor(&self, chosen: usize) -> &Anchor {
        (self.anchors.get(chosen))
            .or_else(|| self.fresh[chosen - self.anchors.len()].get())
            .expect("stored before it is chosen")
    }

    /// Which of the anchors is quickest to find where a byte shows up `count` times.
    fn cheapest(&self, count: impl Fn(u8) -> usize, pass: usize) -> usize {
        (0..self.anchors.len())
            .min_by_key(|&anchor| self.anchors[anchor].cost(&count, pass))
            .expect("the first letter anchors")
    }

    /// The first match. When the anchor shows up all over the first lines read, as E1 does
    /// in Vietnamese for a guess, the bytes last read choose another, which starts over.
    fn find_in(&self, haystack: &[u8]) -> Option<Range<usize>> {
        let chosen = self.chosen.load(Ordering::Acquire);
        let start = self.anchor(chosen);
        let read = match self.search(start, haystack, 0, true) {
            Ok(found) => return found,
            Err(read) => read,
        };
        let sample = &haystack[read - SAMPLE / 2..read];
        let mut counts = [0; 256];
        sample
            .iter()
            .for_each(|&byte| counts[usize::from(byte)] += 1);
        let count = |byte: u8| counts[usize::from(byte)];
        let pass = sample.len() / 250;
        let mut next = self.cheapest(count, pass);
        let mut learned = &self.anchors[next];
        if start.cost(count, pass) <= learned.cost(count, pass) {
            (next, learned) = (chosen, start);
        }
        // Still stopping on a letter beyond ASCII every 64 bytes, as in Vietnamese, where
        // most letters have accents: needles chosen afresh from these bytes.
        let wide = |byte: u8| if byte.is_ascii() { 0 } else { count(byte) };
        if learned.cost(wide, 0) * 64 > sample.len()
            && self.renewals.load(Ordering::Relaxed) < RENEWALS
            && self.attempts.fetch_add(1, Ordering::Relaxed) < 4 * RENEWALS
        {
            let fresh = (self.ways.iter().enumerate())
                .map(|(way, ways)| Anchor::cover(way, ways, count, pass))
                .min_by_key(|fresh| fresh.cost(count, pass))
                .expect("the first letter anchors");
            if fresh.cost(count, pass) < learned.cost(count, pass) {
                let slot = self.renewals.fetch_add(1, Ordering::Relaxed);
                if slot < RENEWALS && self.fresh[slot].set(fresh).is_ok() {
                    next = self.anchors.len() + slot;
                    learned = self.anchor(next);
                }
            }
        }
        self.chosen.store(next, Ordering::Release);
        let from = if std::ptr::eq(learned, start) {
            read
        } else {
            0
        };
        self.search(learned, haystack, from, false)
            .unwrap_or_else(|_| unreachable!("searches to the end"))
    }

    /// The first match from `from`, read in stretches that double, as the next match is often
    /// a line away. `Err` with how far it read when `crowded` and needles stop it too often.
    fn search(
        &self,
        anchor: &Anchor,
        haystack: &[u8],
        mut from: usize,
        crowded: bool,
    ) -> Result<Option<Range<usize>>, usize> {
        // Up to three needles in one pass. More: ASCII spellings first, the way most text
        // writes them, and the others only up to the first match found by those.
        let needles = &anchor.needles;
        let ascii = needles
            .iter()
            .take_while(|needle| needle.is_ascii())
            .count();
        let first = if needles.len() <= 3 || ascii == 0 {
            needles.len().min(3)
        } else {
            ascii.min(3)
        };
        let (first, others) = needles.split_at(first);
        let (mut size, mut tried) = (64, 0);
        while from < haystack.len() {
            let to = haystack.len().min(from + size);
            let window = &haystack[from..to];
            let mut found = |at: usize| {
                tried += stop(haystack[from + at]);
                if !anchor.fits(haystack, from + at) {
                    return None;
                }
                Some((at, self.match_at(anchor, haystack, from + at)?))
            };
            let first = Places::new(first, window).find_map(&mut found);
            let before = first.as_ref().map_or(window.len(), |&(at, _)| at);
            let before = &window[..before];
            // The others are all beyond ASCII.
            let earlier = match others {
                [] => None,
                _ if before.is_ascii() => None,
                others if others.len() <= 3 => Places::new(others, before).find_map(&mut found),
                others => in_order(others, before).find_map(&mut found),
            };
            if let Some((_, found)) = earlier.or(first) {
                return Ok(Some(found));
            }
            if crowded && to >= SAMPLE && tried * 10_000 > to * CROWD {
                return Err(to);
            }
            (from, size) = (to, size * 2);
        }
        Ok(None)
    }

    /// The match whose anchor holds the needle found at `at`, if there is one. A later anchor
    /// means a later start, so the first found is the first in the line.
    #[inline]
    fn match_at(&self, anchor: &Anchor, haystack: &[u8], at: usize) -> Option<Range<usize>> {
        // An ASCII needle is a spelling, and the ASCII bytes on either side of it, as most
        // are, are simply compared. A letter beyond ASCII among them needs folding.
        if self.ascii && haystack[at].is_ascii() {
            let text = self.text.as_bytes();
            let before = self.ways[anchor.way].before;
            let after = (text[before + 1..].iter()).zip(haystack.get(at + 1..).unwrap_or_default());
            let behind = (text[..before].iter().rev()).zip(haystack[..at].iter().rev());
            if self.same_ascii(after, text.len() - before - 1)?
                && self.same_ascii(behind, before)?
            {
                let start = at - before;
                return Some(start..accents_after(haystack, start + text.len()));
            }
        }
        self.match_letter_at(anchor, haystack, at)
    }

    /// Whether `wanted` typed and line bytes are the same ASCII letters: `None` when one differs
    /// or the line runs out, `Some(false)` at a letter that needs folding.
    #[inline]
    fn same_ascii<'a>(
        &self,
        pairs: impl Iterator<Item = (&'a u8, &'a u8)>,
        wanted: usize,
    ) -> Option<bool> {
        let mut read = 0;
        for (&typed, &byte) in pairs {
            if !byte.is_ascii() {
                return Some(false);
            }
            if byte != typed && !(self.any_case && byte.eq_ignore_ascii_case(&typed)) {
                return None;
            }
            read += 1;
        }
        // A letter holds a byte at least for each it folds to, so fewer can't hold the text.
        (read == wanted).then_some(true)
    }

    #[inline(never)]
    fn match_letter_at(&self, anchor: &Anchor, haystack: &[u8], at: usize) -> Option<Range<usize>> {
        let ways = &self.ways[anchor.way];
        let matched = |letter: usize, held: usize| {
            let start = self.start_before(haystack, letter, &self.text[..ways.before - held])?;
            Some(start..self.folded_end(haystack, start)?)
        };
        if haystack[at].is_ascii() {
            return matched(at, 0);
        }
        let letter = crate::letter_around(haystack, at).map_or(at, |letter| letter.start);
        let (spelling, _) = letter_at(haystack, letter)?;
        let here = ways.spellings.binary_search(&spelling).is_ok().then_some(0);
        if ways.inside.is_empty() {
            return matched(letter, here?);
        }
        let inside = ways
            .inside
            .iter()
            .filter(|&&(holder, _)| holder == spelling);
        (here.into_iter().chain(inside.map(|&(_, held)| held)))
            .filter_map(|held| matched(letter, held))
            .min_by_key(|found| found.start)
    }

    /// Where a match starts whose letters before `at` fold to `typed`, the text before the
    /// anchor. They are compared on the way back, so a line that differs stops at once.
    fn start_before(&self, haystack: &[u8], mut at: usize, mut typed: &str) -> Option<usize> {
        let width = |letter: char| {
            if self.any_case {
                letter.to_lowercase().count()
            } else {
                1
            }
        };
        while !typed.is_empty() {
            let (letter, len) = letter_before(haystack, at)?;
            at -= len;
            let mut parts = 0;
            let folded = folds(letter, |part| parts += width(part));
            if !folded {
                parts = width(letter);
            }
            // An accent stored apart from its letter folds to nothing.
            if parts == 0 {
                continue;
            }
            let (start, _) = typed.char_indices().rev().nth(parts - 1)?;
            let mut wanted = typed[start..].chars();
            let mut same = true;
            if folded {
                folds(letter, |part| same &= self.eat(part, &mut wanted));
            } else {
                same = self.eat(letter, &mut wanted);
            }
            if !same {
                return None;
            }
            typed = &typed[..start];
        }
        Some(at)
    }

    /// Where the text ends when it starts at `start`, each letter there folded, with the
    /// accents stored after its last letter. A match starts and ends on whole letters.
    fn folded_end(&self, haystack: &[u8], start: usize) -> Option<usize> {
        let mut wanted = self.text.chars();
        let mut at = start;
        while !wanted.as_str().is_empty() {
            let (letter, len) = letter_at(haystack, at)?;
            let typed = wanted.clone();
            if !self.eat(letter, &mut wanted) {
                wanted = typed;
                let (mut same, mut any) = (true, false);
                let folds = folds(letter, |part| {
                    any = true;
                    same &= self.eat(part, &mut wanted);
                });
                if !folds || !same || (at == start && !any) {
                    return None;
                }
            }
            at += len;
        }
        Some(accents_after(haystack, at))
    }

    /// Takes `letter` off the front of `wanted`, if it is there.
    fn eat(&self, letter: char, wanted: &mut Chars<'_>) -> bool {
        if self.any_case {
            letter
                .to_lowercase()
                .all(|lower| wanted.next() == Some(sigma(lower)))
        } else {
            wanted.next() == Some(letter)
        }
    }
}

/// Where `needles` are in `haystack`, in order, three of them to a pass.
fn in_order<'h>(needles: &[u8], haystack: &'h [u8]) -> impl Iterator<Item = usize> + 'h {
    let (mut passes, mut next) = ([const { None }; MOST_PASSES], [usize::MAX; MOST_PASSES]);
    let count = needles.len().div_ceil(3);
    // More than a search keeps track of, which a test rules out: read a byte at a time.
    let (mut many, mut read) = ([false; 256], (count > MOST_PASSES).then_some(0));
    if read.is_some() {
        needles.iter().for_each(|&n| many[usize::from(n)] = true);
    } else {
        for (pass, three) in needles.chunks(3).enumerate() {
            let mut places = Places::new(three, haystack);
            next[pass] = places.next().unwrap_or(usize::MAX);
            passes[pass] = Some(places);
        }
    }
    iter::from_fn(move || {
        if let Some(from) = &mut read {
            let at = *from
                + haystack[*from..]
                    .iter()
                    .position(|&b| many[usize::from(b)])?;
            *from = at + 1;
            return Some(at);
        }
        let (pass, &at) = next[..count].iter().enumerate().min_by_key(|&(_, at)| at)?;
        let places = passes[pass].as_mut()?;
        next[pass] = places.next().unwrap_or(usize::MAX);
        (at != usize::MAX).then_some(at)
    })
}

/// Where one to three needles are in a haystack, in order.
enum Places<'h> {
    One(memchr::Memchr<'h>),
    Two(memchr::Memchr2<'h>),
    Three(memchr::Memchr3<'h>),
}

impl<'h> Places<'h> {
    fn new(needles: &[u8], haystack: &'h [u8]) -> Self {
        match *needles {
            [a] => Self::One(memchr::memchr_iter(a, haystack)),
            [a, b] => Self::Two(memchr::memchr2_iter(a, b, haystack)),
            [a, b, c] => Self::Three(memchr::memchr3_iter(a, b, c, haystack)),
            _ => unreachable!("chunks of one to three"),
        }
    }
}

impl Iterator for Places<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::One(places) => places.next(),
            Self::Two(places) => places.next(),
            Self::Three(places) => places.next(),
        }
    }
}

/// Lower-case letters that are the same letter written another way.
fn sigma(lower: char) -> char {
    if lower == 'ς' { 'σ' } else { lower }
}

/// Every way to write `wanted`, a letter of the text, sorted: in either case when typed in
/// lower case, and as each letter whose fold starts with it, like ё and ӗ for е.
fn ways_to_write(wanted: char, any_case: bool) -> Vec<char> {
    let (mut all, folded): (Vec<char>, _) = if any_case {
        let all = spellings(wanted).iter().flat_map(|s| s.chars()).collect();
        (all, &folded_letters().by_lower_first)
    } else {
        (vec![wanted], &folded_letters().by_first)
    };
    let start = folded.partition_point(|&(first, _)| first < wanted);
    let folded = folded[start..]
        .iter()
        .take_while(|&&(first, _)| first == wanted);
    all.extend(folded.map(|&(_, letter)| letter));
    all.sort_unstable();
    all.dedup();
    all
}

/// Every letter that lower-cases to `lower`: itself, its upper case, and the odd one out.
fn spellings(lower: char) -> Vec<String> {
    let mut upper = lower.to_uppercase();
    let upper = upper.next().filter(|_| upper.next().is_none());
    let mut all = vec![lower.to_string()];
    for other in [upper, odd_spelling(lower)].into_iter().flatten() {
        if !all.contains(&other.to_string()) {
            all.push(other.to_string());
        }
    }
    all
}

/// A letter that lower-cases to `lower` without being its upper case, like ǅ, the ohm sign or
/// ẞ. A test checks every letter against this list.
fn odd_spelling(lower: char) -> Option<char> {
    Some(match lower {
        'ǆ' => 'ǅ',
        'ǉ' => 'ǈ',
        'ǌ' => 'ǋ',
        'ǳ' => 'ǲ',
        'σ' => 'ς',
        'θ' => '\u{3f4}',
        'ß' => 'ẞ',
        'ω' => '\u{2126}',
        'å' => '\u{212b}',
        'ᾳ' => 'ᾼ',
        'ῃ' => 'ῌ',
        'ῳ' => 'ῼ',
        '\u{1f80}'..='\u{1f87}' | '\u{1f90}'..='\u{1f97}' | '\u{1fa0}'..='\u{1fa7}' => {
            char::from_u32(u32::from(lower) + 8)?
        }
        _ => return None,
    })
}

fn has_case(letter: char) -> bool {
    letter.is_lowercase() || letter.is_uppercase()
}

/// A guess at how many of ten thousand bytes in a line are this one, from English and the
/// languages each lead byte writes; lines where it is wrong, like Vietnamese, are counted.
fn commonness(byte: u8) -> usize {
    const LETTERS: [usize; 26] = [
        650, 120, 230, 330, 1000, 180, 160, 450, 550, 10, 60, 330, 200, 550, 600, 160, 8, 500, 500,
        700, 230, 80, 150, 12, 150, 6,
    ];
    match byte {
        b'a'..=b'z' => LETTERS[usize::from(byte - b'a')],
        b'A'..=b'Z' => LETTERS[usize::from(byte - b'A')] / 10 + 1,
        b' ' => 1600,
        b'0'..=b'9' => 50,
        0..=0x7F => 100,
        0x80..=0xBF => 40,
        0xC3 | 0xD0 | 0xD1 => 300,
        0xCE | 0xCF | 0xE0 => 150,
        0xC4 | 0xC5 | 0xE1 => 50,
        0xE2 => 30,
        0xE3..=0xED => 200,
        _ => 10,
    }
}

impl Matcher for Literal {
    type Captures = NoCaptures;
    type Error = NoError;

    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let found = self.find_in(&haystack[at..]);
        Ok(found.map(|found| Match::new(at + found.start, at + found.end)))
    }

    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }

    /// The text never holds a line break, so whole blocks of lines are searched at once.
    fn line_terminator(&self) -> Option<LineTerminator> {
        Some(LineTerminator::byte(b'\n'))
    }
}

/// Where the letters that fold are: a test checks every other letter.
const FOLDING: [RangeInclusive<u32>; 7] = [
    0x80..=0x2001,
    0x2126..=0x232A,
    0x2ADC..=0x2ADC,
    0xF900..=0xFB4E,
    0x1_05C9..=0x1_05E4,
    0x1_D15E..=0x1_D1C0,
    0x2_F800..=0x2_FA1D,
];

struct Folded {
    /// The first letter each letter that folds into others folds to, and that letter, sorted.
    by_first: Vec<(char, char)>,
    /// The same, by the first letter in lower case.
    by_lower_first: Vec<(char, char)>,
    /// Each letter a fold holds after its first, the letters before it there and the letter
    /// folded, like the second s of ß, "s" and ß, as folded and in lower case, sorted.
    later: Vec<(char, String, char)>,
}

/// The letters that fold, worked out on the first search.
fn folded_letters() -> &'static Folded {
    static ALL: OnceLock<Folded> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all = Folded {
            by_first: Vec::new(),
            by_lower_first: Vec::new(),
            later: Vec::new(),
        };
        for letter in FOLDING.into_iter().flatten().filter_map(char::from_u32) {
            let mut folded = String::new();
            folds(letter, |part| folded.push(part));
            let lower: String = folded.to_lowercase().chars().map(sigma).collect();
            for parts in [&folded, &lower] {
                for (at, part) in parts.char_indices().skip(1) {
                    all.later.push((part, parts[..at].to_string(), letter));
                }
            }
            if let Some(first) = folded.chars().next() {
                all.by_first.push((first, letter));
                let lower = first.to_lowercase().next().map_or(first, sigma);
                all.by_lower_first.push((lower, letter));
            }
        }
        all.by_first.sort_unstable();
        all.by_lower_first.sort_unstable();
        all.later.sort_unstable();
        all.later.dedup();
        all
    })
}

/// `argus_core::fold_letter`, looked up for the letters up to U+1FFF: Latin letters with
/// accents, Greek and Cyrillic among them, which a line can hold thousands of.
fn folds(letter: char, mut each: impl FnMut(char)) -> bool {
    if letter.is_ascii() {
        return false;
    }
    static TABLE: OnceLock<Vec<[char; 2]>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let letters = (0x80..0x2000).map(|code| char::from_u32(code).unwrap());
        letters.map(two_parts).collect()
    });
    let Some(&parts) = table.get(letter as usize - 0x80) else {
        let code = u32::from(letter);
        return FOLDING.iter().any(|range| range.contains(&code))
            && argus_core::fold_letter(letter, each);
    };
    if parts == [letter, '\0'] {
        return false;
    }
    parts
        .into_iter()
        .filter(|&part| part != '\0')
        .for_each(&mut each);
    true
}

/// What a letter folds to, padded with '\0', or itself when it stays. A test checks that none
/// up to U+1FFF folds to more than two.
fn two_parts(letter: char) -> [char; 2] {
    let mut parts = ['\0'; 2];
    let mut n = 0;
    let folds = argus_core::fold_letter(letter, |part| {
        parts[n.min(1)] = part;
        n += 1;
    });
    if folds { parts } else { [letter, '\0'] }
}

/// The letter starting at `at` and its length in bytes, or `None` for a stray byte.
fn letter_at(haystack: &[u8], at: usize) -> Option<(char, usize)> {
    let len = crate::letter_len(*haystack.get(at)?)?;
    let letter = std::str::from_utf8(haystack.get(at..at + len)?).ok()?;
    Some((letter.chars().next()?, len))
}

/// The letter ending at `at` and its length in bytes, or `None` for a stray byte.
fn letter_before(haystack: &[u8], at: usize) -> Option<(char, usize)> {
    let start = (at.saturating_sub(4)..at)
        .rev()
        .find(|&i| !crate::is_inside_letter(haystack[i]))?;
    letter_at(haystack, start).filter(|&(_, len)| start + len == at)
}

/// `at`, moved past any accents stored after the letter before it, like the second half of
/// "e\u{301}".
fn accents_after(haystack: &[u8], mut at: usize) -> usize {
    while haystack.get(at).is_some_and(|byte| !byte.is_ascii())
        && let Some((letter, len)) = letter_at(haystack, at)
        && folds_to_nothing(letter)
    {
        at += len;
    }
    at
}

pub(crate) fn folds_to_nothing(letter: char) -> bool {
    let mut any = false;
    folds(letter, |_| any = true) && !any
}

/// The first match at or after `at`, the slow and obvious way: the text and each letter of
/// the line folded on its own, and lower-cased when the text has no capital.
#[cfg(test)]
pub(crate) fn plain_find(text: &str, haystack: &str, at: usize) -> Option<(usize, usize)> {
    let fold = |letter: char| {
        let mut folded = Vec::new();
        if !argus_core::fold_letter(letter, |part| folded.push(part)) {
            folded.push(letter);
        }
        folded
    };
    let text: Vec<char> = text.chars().flat_map(fold).collect();
    let any_case = text.iter().any(|&l| has_case(l)) && !text.iter().any(|l| l.is_uppercase());
    let case = |letters: Vec<char>| -> Vec<char> {
        if any_case {
            let lower = letters.iter().flat_map(|letter| letter.to_lowercase());
            lower.map(sigma).collect()
        } else {
            letters
        }
    };
    let wanted = case(text);
    let letters: Vec<(usize, Vec<char>)> = haystack
        .char_indices()
        .map(|(start, letter)| (start, case(fold(letter))))
        .collect();
    let start_of = |i: usize| letters.get(i).map_or(haystack.len(), |l| l.0);
    (0..letters.len())
        .filter(|&i| letters[i].0 >= at && !letters[i].1.is_empty())
        .find_map(|i| {
            let mut got: Vec<char> = Vec::new();
            let mut end = i;
            while got.len() < wanted.len() && end < letters.len() {
                got.extend(&letters[end].1);
                end += 1;
            }
            while end < letters.len() && letters[end].1.is_empty() {
                end += 1;
            }
            (got == wanted).then(|| (start_of(i), start_of(end)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, haystack: &str) -> Option<(usize, usize)> {
        let literal = Literal::new(text).unwrap();
        let found = literal.find(haystack.as_bytes()).unwrap();
        found.map(|m| (m.start(), m.end()))
    }

    #[test]
    fn picks_exact_or_any_case_from_how_the_text_is_typed() {
        let any_case = |text| Literal::new(text).map(|literal| literal.any_case);
        assert_eq!(any_case("Hello"), Some(false));
        assert_eq!(any_case("12.5%"), Some(false));
        assert_eq!(any_case("hello"), Some(true));
        assert_eq!(any_case("école"), Some(true));
        assert_eq!(any_case("École"), Some(false));
        assert_eq!(any_case("привет"), Some(true));
        assert_eq!(any_case("Привет"), Some(false));
        assert_eq!(any_case(""), None);
        assert_eq!(any_case("a\nb"), None);
        assert_eq!(any_case("\u{301}"), None);
    }

    #[test]
    fn finds_the_first_match_from_where_it_is_asked() {
        let literal = Literal::new("ab").unwrap();
        let found = literal.find_at(b"ab xAB", 1).unwrap().unwrap();
        assert_eq!((found.start(), found.end()), (4, 6));
        assert_eq!(find("milk", "MILK and milk"), Some((0, 4)));
        assert_eq!(find("Milk", "MILK and Milk"), Some((9, 13)));
        assert_eq!(find("milk", "mil"), None);
        assert_eq!(find("k", "K"), Some((0, 1)));
    }

    #[test]
    fn the_rare_byte_may_sit_at_either_end() {
        assert_eq!(find("zebra", "a ZEBRA"), Some((2, 7)));
        assert_eq!(find("eez", "eeeeez"), Some((3, 6)));
        assert_eq!(find("eez", "eez"), Some((0, 3)));
        assert_eq!(find("eez", "ez"), None);
    }

    #[test]
    fn letters_beyond_ascii_match_any_case() {
        assert_eq!(find("école", "L'ÉCOLE"), Some((2, 8)));
        assert_eq!(find("straße", "STRAẞE"), Some((0, 8)));
        assert_eq!(find("привет", "ПРИВЕТ мир"), Some((0, 12)));
        // Bytes that are not text are stepped over.
        let literal = Literal::new("é").unwrap();
        let found = literal.find(b"\xff\xfe \xc3\x89").unwrap().unwrap();
        assert_eq!((found.start(), found.end()), (3, 5));
    }

    /// The bold range covers the letters as the line stores them, the accent included.
    #[test]
    fn accents_count_on_neither_side() {
        let (line, plain) = ("Le café est fermé", "cafe hours");
        for text in [
            "cafe",
            "café",
            "cafe\u{301}",
            "CAFÉ".to_lowercase().as_str(),
        ] {
            assert_eq!(find(text, line), Some((3, 8)), "{text:?}");
            assert_eq!(find(text, plain), Some((0, 4)), "{text:?}");
            assert_eq!(find(text, "Le cafe\u{301} est"), Some((3, 9)), "{text:?}");
        }
        assert_eq!(find("ferme", line), Some((13, 19)));
        assert_eq!(find("Café", "CAFE Cafe\u{301}"), Some((5, 11)));
        assert_eq!(find("cafes", "cafe\u{301}s"), Some((0, 7)));
        assert_eq!(find("fermé", "ferme\u{301}\u{301}!"), Some((0, 9)));
        assert_eq!(find("ecole", "L'École"), Some((2, 8)));
        assert_eq!(find("ёлка", "Елка и ёлка"), Some((0, 8)));
        assert_eq!(find("елка", "ЁЛКА"), Some((0, 8)));
        assert_eq!(find("Ёлка", "елка Елка"), Some((9, 17)));
        assert_eq!(find("мой", "мои\u{306}"), Some((0, 8)));
        assert_eq!(find("οδος", "Ο δρόμος, ΟΔΌΣ"), Some((17, 25)));
        assert_eq!(find("strasse", "Straße"), Some((0, 7)));
        // A match starts on a letter, never on an accent left over from the one before.
        assert_eq!(find("e", "\u{301}e"), Some((2, 3)));
        assert_eq!(find("café", "cafard"), None);
    }

    /// Kana and Hangul pasted as a file name stores them, mark apart, find those bytes, but
    /// not the joined letter.
    #[test]
    fn marks_stored_apart_find_the_same_bytes() {
        let (ga, han) = ("\u{304b}\u{3099}", "\u{1112}\u{1161}\u{11ab}");
        assert_eq!(find(ga, &format!("x {ga}っこう")), Some((2, 8)));
        assert_eq!(find(han, &format!("{han}국")), Some((0, 9)));
        assert_eq!(find("\u{304c}", ga), None);
        assert_eq!(find("\u{d55c}", han), None);
    }

    #[test]
    fn other_spellings_of_a_letter_match_too() {
        assert_eq!(find("οδός", "ΟΔΌΣ"), Some((0, 8)));
        assert_eq!(find("λογοσ", "λογος"), Some((0, 10)));
        assert_eq!(find("ßπ", "ẞΠ"), Some((0, 5)));
        assert_eq!(find("ǆa", "ǅA ǄA"), Some((0, 3)));
        assert_eq!(find("ωμ", "x \u{2126}Μ"), Some((2, 7)));
        assert_eq!(find("ßé", "ß ẞé"), Some((3, 8)));
        assert_eq!(find("ßé", "ẞé ßé"), Some((0, 5)));
    }

    /// Signs that stand for a letter fold to it, as in names.
    #[test]
    fn a_sign_for_a_letter_matches_the_letter() {
        let kelvin = "\u{212a}";
        assert_eq!(find("kπ", &format!("{kelvin}Π")), Some((0, 5)));
        assert_eq!(find("πk", &format!("Π{kelvin}")), Some((0, 5)));
        assert_eq!(find("k", kelvin), Some((0, 3)));
        assert_eq!(find("K", kelvin), Some((0, 3)));
        assert_eq!(find(kelvin, "k"), None);
        assert_eq!(find("iš", "İŠ"), Some((0, 4)));
    }

    /// Found in few passes, and not by a byte their whole alphabet starts with. "the" takes
    /// three, as each of its letters is written many ways.
    #[test]
    fn common_words_are_found_in_few_passes() {
        for (text, passes) in [
            ("Zebra", 1),
            ("zebra", 2),
            ("the", 3),
            ("cafe", 1),
            ("café", 1),
            ("жук", 1),
            ("привет", 1),
            ("Привет", 1),
            ("οδός", 2),
            ("東京大学", 1),
            ("한국어사전", 1),
        ] {
            let literal = Literal::new(text).unwrap();
            let needles = &literal
                .anchor(literal.chosen.load(Ordering::Relaxed))
                .needles;
            assert!(
                needles.len().div_ceil(3) <= passes,
                "{text:?}: {needles:x?}"
            );
            let mut alphabet = text
                .chars()
                .map(|l| l.encode_utf8(&mut [0; 4]).as_bytes()[0]);
            assert!(
                !alphabet.any(|lead| lead > 0x7F && needles.contains(&lead)),
                "{text:?}"
            );
        }
    }

    /// Lines full of the guessed anchor's bytes choose another, which finds the same.
    #[test]
    fn crowded_lines_choose_another_anchor() {
        for (text, filler, tail) in [
            ("Zebra", "ạ ", "x Zẹbra"),
            ("zebra", "ž", " ZEBRA"),
            ("cafe", "ḟ", " café"),
            ("xyzzy", "ẍạỹ ", " XYZZY"),
            ("the", "þ ", "THE"),
        ] {
            let literal = Literal::new(text).unwrap();
            let haystack = format!("{}{tail}", filler.repeat(SAMPLE));
            let guess = literal.anchor(literal.chosen.load(Ordering::Relaxed));
            let guess = literal.search(guess, haystack.as_bytes(), 0, true);
            assert!(guess.is_err(), "{text:?}");
            let found = literal.find(haystack.as_bytes()).unwrap();
            let found = found.map(|m| (m.start(), m.end()));
            assert!(found.is_some(), "{text:?}");
            assert_eq!(found, plain_find(text, &haystack, 0), "{text:?}");
        }
    }

    /// The h of "the" is also looked for as the þ that holds it after the t, and the e of
    /// "mae" as an æ; the s of "es" is in no letter after an e.
    #[test]
    fn a_letter_is_looked_for_in_the_letters_that_hold_it() {
        let inside = |text: &str, at: usize| -> Vec<(char, usize)> {
            let literal = Literal::new(text).unwrap();
            literal.ways[at].inside.clone()
        };
        assert_eq!(inside("the", 1), [('Þ', 1), ('þ', 1)]);
        assert!(inside("mae", 2).contains(&('æ', 1)));
        assert!(inside("mae", 2).contains(&('Ǽ', 1)));
        assert_eq!(inside("es", 1), []);
        assert_eq!(inside("ss", 0), []);
        assert!(inside("ss", 1).contains(&('ß', 1)));
        assert_eq!(find("the", "x þe"), Some((2, 5)));
        assert_eq!(find("the", "x tþe"), Some((3, 6)));
        assert_eq!(find("the", "þ e"), None);
        assert_eq!(find("mae", "MÆ"), Some((0, 3)));
        assert_eq!(find("ss", "ß"), Some((0, 2)));
        assert_eq!(find("sss", "sß"), Some((0, 3)));
    }

    /// No letter takes more passes than a search keeps track of, whichever bytes are rare.
    #[test]
    fn no_letter_takes_too_many_passes() {
        let fits = |ways: &Ways, anchors: &[Anchor]| {
            let costs = [commonness, |_| 0, |byte: u8| usize::from(byte & 0x3F)];
            let covers = costs.map(|cost| Anchor::cover(0, ways, cost, PASS));
            for anchor in anchors
                .iter()
                .chain(&covers)
                .chain([&Anchor::firsts(0, ways)])
            {
                let needles = &anchor.needles;
                assert!(
                    needles.len() <= 3 * MOST_PASSES,
                    "{:?}: {needles:x?}",
                    ways.spellings
                );
            }
        };
        // Each letter alone and with every letter that can hold it, and in each text that
        // ends in a held letter, as `Literal::new` builds them.
        let all = folded_letters();
        let firsts = all.by_first.iter().map(|&(first, _)| first);
        let letters = firsts.chain(all.later.iter().map(|&(part, ..)| part));
        for letter in letters.flat_map(|letter| letter.to_lowercase().chain([letter])) {
            let start = all.later.partition_point(|(part, ..)| *part < letter);
            let holders = all.later[start..]
                .iter()
                .take_while(|(part, ..)| *part == letter);
            let holders: Vec<(char, usize)> = holders.map(|&(_, _, holder)| (holder, 1)).collect();
            for any_case in [false, true] {
                let spellings = ways_to_write(letter, any_case);
                for inside in [Vec::new(), holders.clone()] {
                    let spellings = spellings.clone();
                    fits(
                        &Ways {
                            before: 0,
                            spellings,
                            inside,
                        },
                        &[],
                    );
                }
            }
        }
        for (part, earlier, _) in &all.later {
            for text in [
                format!("{earlier}{part}"),
                format!("{earlier}{part}").to_uppercase(),
            ] {
                let literal = Literal::new(&text).unwrap();
                literal
                    .ways
                    .iter()
                    .for_each(|ways| fits(ways, &literal.anchors));
            }
        }
        // Were there more, they would still be found, a byte at a time.
        let many: Vec<u8> = (0..3 * MOST_PASSES as u8 + 2).collect();
        assert_eq!(in_order(&many, b"\x19 a\x01").collect::<Vec<_>>(), [0, 3]);
    }

    /// Every spelling of a letter written many ways is found, however many passes they take.
    #[test]
    fn letters_written_many_ways_are_found_in_each() {
        for (text, letter) in [("a", 'a'), ("A", 'A'), ("е", 'е'), ("ω", 'ω')] {
            let ways = ways_to_write(letter, text.chars().all(char::is_lowercase));
            let haystack: String = ways.iter().map(|way| format!("{way} ")).collect();
            let literal = Literal::new(text).unwrap();
            let (mut at, mut found) = (0, 0);
            loop {
                let got = literal.find_at(haystack.as_bytes(), at).unwrap();
                let got = got.map(|m| (m.start(), m.end()));
                assert_eq!(got, plain_find(text, &haystack, at), "{text:?} from {at}");
                let Some((_, end)) = got else { break };
                (at, found) = (end, found + 1);
            }
            assert!(found > ways.len() / 2, "{text:?}");
        }
    }

    /// Every letter that lower-cases to the first letter of the text is looked for, or a line
    /// spelling it that way would be passed over.
    #[test]
    fn every_spelling_of_a_letter_is_looked_for() {
        for letter in (0..=0x10_ffff).filter_map(char::from_u32) {
            let mut lower = letter.to_lowercase();
            let (Some(lower), None) = (lower.next(), lower.next()) else {
                continue;
            };
            if lower.is_ascii() && !letter.is_ascii() {
                continue;
            }
            let spellings = spellings(sigma(lower));
            assert!(spellings.len() <= 3, "{lower:?}");
            assert!(spellings.contains(&letter.to_string()), "{letter:?}");
        }
    }

    /// Every letter that folds is known, or lines spelling a letter of the text that way
    /// would be passed over.
    #[test]
    fn every_letter_that_folds_is_looked_for() {
        let known = &folded_letters().by_first;
        for letter in (0x80..=0x10_ffff).filter_map(char::from_u32) {
            let mut first = None;
            let mut parts = 0;
            if !argus_core::fold_letter(letter, |part| {
                first.get_or_insert(part);
                parts += 1;
            }) {
                continue;
            }
            let code = u32::from(letter);
            assert!(FOLDING.iter().any(|r| r.contains(&code)), "{letter:?}");
            let found = first.map(|first| known.binary_search(&(first, letter)));
            assert!(found.is_none_or(|found| found.is_ok()), "{letter:?}");
            assert!(code >= 0x2000 || parts <= 2, "{letter:?}");
        }
    }

    #[test]
    fn letters_up_to_u1fff_fold_as_the_core_folds_them() {
        for letter in (0x80..0x2000).filter_map(char::from_u32) {
            let (mut looked_up, mut folded) = (Vec::new(), Vec::new());
            let changes = argus_core::fold_letter(letter, |part| folded.push(part));
            assert_eq!(folds(letter, |part| looked_up.push(part)), changes);
            assert_eq!(looked_up, folded, "{letter:?}");
        }
    }

    /// Random haystacks, some long and some crowded so another anchor is chosen, find what a
    /// plain letter-by-letter comparison finds.
    #[test]
    fn any_case_agrees_with_a_plain_search() {
        let mut random = crate::tests::random_below(0x9e37_79b9_7f4a_7c15);
        let alphabets: [&[&str]; 6] = [
            &[
                "s", "S", "ß", "ẞ", "π", "Π", "a", "e", "æ", "Æ", "\u{301}", "α", "ά", "ᾶ", " ",
            ],
            &["a", "b", "A", "B", " ", "z", "."],
            &[
                "a", "A", "é", "É", "ß", "ẞ", "σ", "ς", "Σ", "\u{2126}", "ω", "k", "\u{212a}", " ",
            ],
            &[
                "e", "é", "É", "\u{301}", "\u{323}", "ệ", "s", "ß", "ẞ", "c", "ç", "й", "и",
                "\u{306}", "Й", "ё", "е", " ",
            ],
            &[
                "κ", "Κ", "α", "ά", "ἀ", "ᾶ", "\u{345}", "ι", "ς", "σ", "Σ", "t", "h", "þ", "Þ",
                "œ", "ø",
            ],
            &[
                "\u{304b}", "\u{304c}", "\u{3099}", "\u{1112}", "\u{1161}", "\u{d55c}", "a",
                "\u{301}", " ",
            ],
        ];
        for alphabet in alphabets {
            for _ in 0..5000 {
                let (long, crowded) = (random(8) == 0, random(16) == 0);
                let mut pick = |most: usize| -> String {
                    let len = random(most);
                    (0..len).map(|_| alphabet[random(alphabet.len())]).collect()
                };
                let crowd = if crowded {
                    pick(4).repeat(SAMPLE / 2)
                } else {
                    String::new()
                };
                let haystack = crowd + &pick(if long { 600 } else { 40 });
                let text = pick(5);
                let text = if random(3) == 0 {
                    text
                } else {
                    text.to_lowercase()
                };
                let Some(literal) = Literal::new(&text) else {
                    continue;
                };
                let mut at = random(haystack.len() + 1);
                while !haystack.is_char_boundary(at) {
                    at -= 1;
                }
                let found = literal.find_at(haystack.as_bytes(), at).unwrap();
                let found = found.map(|m| (m.start(), m.end()));
                assert_eq!(
                    found,
                    plain_find(&text, &haystack, at),
                    "{text:?} in {haystack:?} from {at}"
                );
            }
        }
    }
}
