//! The typed text, found letter for letter: upper-case letters must match exactly, and text
//! typed all in lower case matches any case.

use grep_matcher::{LineMatchKind, LineTerminator, Match, Matcher, NoCaptures, NoError};
use memchr::memmem::Finder;

pub(crate) enum Literal {
    Exact(Finder<'static>),
    /// ASCII text, lower-cased, and the place of its rarest byte, which is looked for first.
    AnyCaseAscii {
        text: Vec<u8>,
        rare: usize,
    },
    /// Text with other letters, lower-cased, and what to look for first: its longest ASCII
    /// run, or the last byte of its first letter in either case.
    AnyCase {
        text: String,
        ascii: Option<(Vec<u8>, usize)>,
        first_ends: [u8; 2],
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
                let text = text.to_lowercase();
                let run = text
                    .as_bytes()
                    .split(|byte| !byte.is_ascii())
                    .max_by_key(|run| run.len())
                    .filter(|run| !run.is_empty());
                let first = text.chars().next().unwrap_or_default();
                let mut upper = first.to_uppercase();
                let upper = upper.next().filter(|_| upper.next().is_none());
                Self::AnyCase {
                    ascii: run.map(|run| (run.to_vec(), rarest(run))),
                    first_ends: [last_byte(first), last_byte(upper.unwrap_or(first))],
                    text,
                }
            }
        })
    }
}

fn last_byte(letter: char) -> u8 {
    let mut bytes = [0; 4];
    *letter
        .encode_utf8(&mut bytes)
        .as_bytes()
        .last()
        .unwrap_or(&0)
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
            Self::AnyCase { text, .. } => find_any_case(rest, text),
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

    /// A line without the ASCII run, like "cole" of "école", or without a byte of the first
    /// letter cannot match, so only lines holding it are checked letter by letter.
    fn find_candidate_line(&self, haystack: &[u8]) -> Result<Option<LineMatchKind>, NoError> {
        let Self::AnyCase {
            ascii, first_ends, ..
        } = self
        else {
            return Ok(self.shortest_match(haystack)?.map(LineMatchKind::Confirmed));
        };
        let found = match ascii {
            Some((run, rare)) => find_ascii(haystack, run, *rare).map(|found| found.start),
            None => memchr::memchr2(first_ends[0], first_ends[1], haystack),
        };
        Ok(found.map(LineMatchKind::Candidate))
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

/// Compared a letter at a time, each lower-cased: a slower path, taken only for text like
/// "école" typed in lower case.
fn find_any_case(haystack: &[u8], text: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    for chunk in haystack.utf8_chunks() {
        let valid = chunk.valid();
        for (start, _) in valid.char_indices() {
            if let Some(len) = folded_prefix(&valid[start..], text) {
                return Some(offset + start..offset + start + len);
            }
        }
        offset += valid.len() + chunk.invalid().len();
    }
    None
}

/// How many bytes of `haystack` match the lower-cased `text` from its start.
fn folded_prefix(haystack: &str, text: &str) -> Option<usize> {
    let mut wanted = text.chars();
    for (at, letter) in haystack.char_indices() {
        for lower in letter.to_lowercase() {
            if wanted.next() != Some(lower) {
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
    fn a_line_without_the_ascii_byte_is_never_a_candidate() {
        let literal = Literal::new("école").unwrap();
        assert!(matches!(
            literal.find_candidate_line(b"code\nxCoLe").unwrap(),
            Some(LineMatchKind::Candidate(6))
        ));
        assert!(literal.find_candidate_line(b"code cola").unwrap().is_none());
        let no_ascii = Literal::new("привет").unwrap();
        assert!(matches!(
            no_ascii
                .find_candidate_line("a\nПРИВЕТ".as_bytes())
                .unwrap(),
            Some(LineMatchKind::Candidate(3))
        ));
        assert!(matches!(
            no_ascii.find_candidate_line("x привет".as_bytes()).unwrap(),
            Some(LineMatchKind::Candidate(3))
        ));
        assert!(
            no_ascii
                .find_candidate_line(b"code only")
                .unwrap()
                .is_none()
        );
    }

    /// Random haystacks: the fast any-case search finds what a plain lower-case comparison
    /// finds, at the same place.
    #[test]
    fn ascii_any_case_agrees_with_a_plain_search() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % below as u64).unwrap()
        };
        let alphabet = b"abAB z.";
        for _ in 0..5000 {
            let haystack: Vec<u8> = (0..random(40)).map(|_| alphabet[random(7)]).collect();
            let text: Vec<u8> = (0..=random(4)).map(|_| alphabet[random(7)]).collect();
            let text = String::from_utf8(text).unwrap().to_ascii_lowercase();
            let Some(literal) = Literal::new(&text) else {
                continue;
            };
            let expected = haystack
                .to_ascii_lowercase()
                .windows(text.len())
                .position(|window| window == text.as_bytes());
            let found = literal.find(&haystack).unwrap().map(|m| m.start());
            assert_eq!(found, expected, "{text:?} in {haystack:?}");
        }
    }
}
