//! The typed text, found letter for letter: upper-case letters must match exactly, and text
//! typed all in lower case matches any case. An ASCII letter in the text matches only ASCII
//! letters, so the Kelvin sign is not a "k".

use grep_matcher::{LineTerminator, Match, Matcher, NoCaptures, NoError};
use memchr::memmem::Finder;

#[allow(
    clippy::large_enum_variant,
    reason = "one per search; x86-64 finders are larger, and boxing adds a jump to every find"
)]
pub(crate) enum Literal {
    Exact(Finder<'static>),
    /// ASCII text, lower-cased, and the place of its rarest byte, which is looked for first.
    AnyCaseAscii {
        text: Vec<u8>,
        rare: usize,
    },
    /// Text with other letters, lower-cased with a final ς as σ, and each way its first
    /// letter can be written, found by their last bytes.
    AnyCase {
        text: String,
        firsts: Vec<String>,
        ends: [u8; 3],
    },
}

impl Literal {
    /// `None` for empty text, or text that runs over two lines.
    pub(crate) fn new(text: &str) -> Option<Self> {
        if text.is_empty() || text.contains('\n') {
            return None;
        }
        let any_case = text.chars().any(has_case) && !text.chars().any(char::is_uppercase);
        Some(match () {
            () if !any_case => Self::Exact(Finder::new(text).into_owned()),
            () if text.is_ascii() => {
                let text = text.as_bytes().to_ascii_lowercase();
                Self::AnyCaseAscii {
                    rare: rarest(&text),
                    text,
                }
            }
            () => {
                let text: String = text.to_lowercase().chars().map(fold).collect();
                let firsts = spellings(text.chars().next().unwrap_or_default());
                let end = |i: usize| *firsts[i.min(firsts.len() - 1)].as_bytes().last().unwrap();
                Self::AnyCase {
                    ends: [end(0), end(1), end(2)],
                    firsts,
                    text,
                }
            }
        })
    }
}

/// Lower-case letters that are the same letter written another way.
fn fold(lower: char) -> char {
    if lower == 'ς' { 'σ' } else { lower }
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

/// A letter that lower-cases to `lower` without being its upper case: a title-case pair like
/// ǅ, a sign standing for a letter like the ohm sign, or ẞ, whose lower case upper-cases to
/// "SS". A test checks every letter against this list.
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

fn rarest(text: &[u8]) -> usize {
    (0..text.len())
        .min_by_key(|&i| commonness(text[i]))
        .unwrap_or(0)
}

fn has_case(letter: char) -> bool {
    letter.is_lowercase() || letter.is_uppercase()
}

/// Roughly how often a byte shows up in text and code, so the rarest is looked for first.
fn commonness(byte: u8) -> usize {
    const LETTERS: &[u8] = b"etaoinsrhldcumfpgwybvkxjqz";
    match byte.to_ascii_lowercase() {
        b' ' => 100,
        letter @ b'a'..=b'z' => 90 - LETTERS.iter().position(|&l| l == letter).unwrap_or(0) * 3,
        b'0'..=b'9' | b'.' | b',' | b'_' | b'(' | b')' | b'"' | b'=' | b'-' => 20,
        _ => 10,
    }
}

impl Matcher for Literal {
    type Captures = NoCaptures;
    type Error = NoError;

    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let rest = &haystack[at..];
        let found = match self {
            Self::Exact(finder) => finder
                .find(rest)
                .map(|start| start..start + finder.needle().len()),
            Self::AnyCaseAscii { text, rare } => find_ascii(rest, text, *rare),
            Self::AnyCase { text, firsts, ends } => find_any_case(rest, text, firsts, *ends),
        };
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

fn find_ascii(haystack: &[u8], text: &[u8], rare: usize) -> Option<std::ops::Range<usize>> {
    let (lower, upper) = (text[rare], text[rare].to_ascii_uppercase());
    let after = haystack.get(rare..)?;
    memchr::memchr2_iter(lower, upper, after)
        .map(|start| start..start + text.len())
        .find(|found| {
            haystack
                .get(found.clone())
                .is_some_and(|window| window.eq_ignore_ascii_case(text))
        })
}

/// Text like "école" typed in lower case: each place holding a spelling of its first letter
/// is compared a letter at a time.
fn find_any_case(
    haystack: &[u8],
    text: &str,
    firsts: &[String],
    ends: [u8; 3],
) -> Option<std::ops::Range<usize>> {
    // Each matched letter stands for one letter of the text and is at most four bytes long.
    let longest = 4 * text.chars().count();
    let mut best: Option<std::ops::Range<usize>> = None;
    for end in memchr::memchr3_iter(ends[0], ends[1], ends[2], haystack) {
        // Spellings differ in length, so a later end can still mean an earlier start.
        if best.as_ref().is_some_and(|best| end >= best.start + 4) {
            break;
        }
        for first in firsts {
            let Some(start) = (end + 1).checked_sub(first.len()) else {
                continue;
            };
            if &haystack[start..=end] != first.as_bytes()
                || best.as_ref().is_some_and(|best| best.start <= start)
            {
                continue;
            }
            let window = &haystack[start..haystack.len().min(start + longest)];
            let valid = window
                .utf8_chunks()
                .next()
                .map_or("", |chunk| chunk.valid());
            if let Some(len) = folded_prefix(valid, text) {
                best = Some(start..start + len);
            }
        }
    }
    best
}

/// How many bytes of `haystack` match the lower-cased `text` from its start.
fn folded_prefix(haystack: &str, text: &str) -> Option<usize> {
    let mut wanted = text.chars();
    for (at, letter) in haystack.char_indices() {
        for lower in letter.to_lowercase() {
            if wanted.next() != Some(fold(lower)) || (lower.is_ascii() && !letter.is_ascii()) {
                return None;
            }
        }
        if wanted.as_str().is_empty() {
            return Some(at + letter.len_utf8());
        }
    }
    None
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
        assert!(matches!(Literal::new("Hello"), Some(Literal::Exact(_))));
        assert!(matches!(Literal::new("12.5%"), Some(Literal::Exact(_))));
        assert!(matches!(
            Literal::new("hello"),
            Some(Literal::AnyCaseAscii { .. })
        ));
        assert!(matches!(
            Literal::new("école"),
            Some(Literal::AnyCase { .. })
        ));
        assert!(matches!(Literal::new("École"), Some(Literal::Exact(_))));
        assert!(Literal::new("").is_none());
        assert!(Literal::new("a\nb").is_none());
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
        assert_eq!(find("café", "cafe"), None);
        // Bytes that are not text are stepped over.
        let literal = Literal::new("é").unwrap();
        let found = literal.find(b"\xff\xfe \xc3\x89").unwrap().unwrap();
        assert_eq!((found.start(), found.end()), (3, 5));
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

    #[test]
    fn an_ascii_letter_matches_only_ascii() {
        let kelvin = "\u{212a}";
        assert_eq!(find("kπ", &format!("{kelvin}Π")), None);
        assert_eq!(find("πk", &format!("Π{kelvin}")), None);
        assert_eq!(find("k", kelvin), None);
        assert_eq!(find("iš", "İŠ"), None);
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
            let spellings = spellings(fold(lower));
            assert!(spellings.len() <= 3, "{lower:?}");
            assert!(spellings.contains(&letter.to_string()), "{letter:?}");
        }
    }

    /// A fixed-seed xorshift, so a failing case comes back on every run.
    fn random_below(mut seed: u64) -> impl FnMut(usize) -> usize {
        move |below| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % below as u64).unwrap()
        }
    }

    /// The first match at or after `at`, by lower-casing each letter on its own.
    fn plain_find(text: &str, haystack: &str, at: usize) -> Option<(usize, usize)> {
        let wanted: Vec<char> = text.to_lowercase().chars().map(fold).collect();
        let letters: Vec<(usize, char)> = haystack.char_indices().collect();
        (0..letters.len())
            .filter(|&i| letters[i].0 >= at)
            .find_map(|i| {
                let got = letters.get(i..i + wanted.len())?;
                let same = got.iter().zip(&wanted).all(|(&(_, letter), &want)| {
                    let lower = letter.to_lowercase().map(fold).collect::<Vec<_>>();
                    lower == [want] && (letter.is_ascii() || !want.is_ascii())
                });
                let end = letters
                    .get(i + wanted.len())
                    .map_or(haystack.len(), |l| l.0);
                same.then_some((letters[i].0, end))
            })
    }

    /// Random haystacks, searched from a random place: the fast any-case searches find what a
    /// plain letter-by-letter comparison finds, at the same place and length.
    #[test]
    fn any_case_agrees_with_a_plain_search() {
        let mut random = random_below(0x9e37_79b9_7f4a_7c15);
        let alphabets: [&[&str]; 2] = [
            &["a", "b", "A", "B", " ", "z", "."],
            &[
                "a", "A", "é", "É", "ß", "ẞ", "σ", "ς", "Σ", "\u{2126}", "ω", "k", "\u{212a}", " ",
            ],
        ];
        for alphabet in alphabets {
            for _ in 0..5000 {
                let mut pick = |most: usize| -> String {
                    let len = random(most);
                    (0..len).map(|_| alphabet[random(alphabet.len())]).collect()
                };
                let haystack = pick(40);
                let text = pick(5).to_lowercase();
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
