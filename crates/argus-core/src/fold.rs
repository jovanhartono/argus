//! The text a name is matched by.

use std::borrow::Cow;
use std::ops::RangeInclusive;
use unicode_normalization::char::decompose_canonical;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

const ACCENTS: RangeInclusive<char> = '\u{300}'..='\u{36f}';

/// Accents off, and é made one character whichever way the disk stores it, so "resume" and
/// "côte" find Résumé.pdf and a Côte map.svg a browser saved as o and an accent. Case is kept,
/// as a capital typed minds case. Borrowed whenever nothing changes.
pub(crate) fn fold(text: &str) -> Cow<'_, str> {
    // A home of Japanese or Russian names has nothing to fold, and the full pass below made
    // opening its index three times slower.
    if text.is_ascii() || stays(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.nfd().filter(|c| !ACCENTS.contains(c)).nfc() {
        match spelled(c) {
            Some(letters) => out.push_str(letters),
            None => out.push(c),
        }
    }
    // A lone accent folds to nothing, and an empty word would match every name.
    if out.is_empty() || out == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// Whether `fold` leaves `text` as it is: already one character per accented letter, and with
/// no accent or spelled-out letter in it.
fn stays(text: &str) -> bool {
    text.chars().all(|c| {
        let mut changes = spelled(c).is_some();
        decompose_canonical(c, |part| changes |= ACCENTS.contains(&part));
        !changes
    }) && is_nfc_quick(text.chars()) == IsNormalized::Yes
}

/// Letters with no accent to take off, as someone on a plain keyboard spells them.
fn spelled(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' => "ss",
        'ẞ' => "SS",
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ø' => "o",
        'Ø' => "O",
        'đ' | 'ð' => "d",
        'Đ' | 'Ð' => "D",
        'ł' => "l",
        'Ł' => "L",
        'þ' => "th",
        'Þ' => "TH",
        'ı' => "i",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_form_folds_the_same() {
        let cases = [
            ("Re\u{301}sume\u{301}.pdf", "Resume.pdf"),
            ("R\u{e9}sum\u{e9}.pdf", "Resume.pdf"),
            ("Co\u{302}te map.svg", "Cote map.svg"),
            ("cr\u{e8}me br\u{fb}l\u{e9}e.txt", "creme brulee.txt"),
            ("Stra\u{df}e", "Strasse"),
            ("\u{c6}r\u{f8}", "AEro"),
            ("Vie\u{323}\u{302}t", "Viet"),
            ("Vi\u{1ec7}t", "Viet"),
            ("I\u{307}stanbul", "Istanbul"),
            ("\u{130}stanbul", "Istanbul"),
            ("\u{304b}\u{3099}っこう", "\u{304c}っこう"),
            ("\u{30cf}\u{309a}", "\u{30d1}"),
            ("\u{1112}\u{1161}\u{11ab}", "\u{d55c}"),
            ("\u{dc}ber", "Uber"),
        ];
        for (text, folded) in cases {
            assert_eq!(fold(text), folded, "{text:?}");
        }
        // Voiced kana stay letters of their own, and a lone accent is never an empty word.
        for kept in [
            "\u{304c}",
            "\u{30cf}",
            "\u{30d0}",
            "会議 report.md",
            "2024 → 2025 – plan.key",
            "readme.md",
            "\u{301}",
        ] {
            assert!(
                matches!(fold(kept), Cow::Borrowed(same) if same == kept),
                "{kept:?}"
            );
        }
    }

    #[test]
    fn fold_ignores_the_storage_form() {
        let parts = [
            "e",
            "E",
            "\u{e9}",
            "e\u{301}",
            "\u{c9}",
            "\u{df}",
            "\u{304c}",
            "\u{304b}\u{3099}",
            "\u{d55c}",
            "\u{1112}\u{1161}\u{11ab}",
            "日",
            "→",
            " ",
            ".",
            "\u{439}",
            "\u{438}\u{306}",
        ];
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        for _ in 0..20_000 {
            let text: String = (0..seed % 10)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    parts[(seed % parts.len() as u64) as usize]
                })
                .collect();
            let (nfc, nfd): (String, String) = (text.nfc().collect(), text.nfd().collect());
            assert_eq!(fold(&nfc), fold(&nfd), "{text:?}");
            match fold(&text) {
                Cow::Borrowed(_) => {}
                Cow::Owned(folded) => assert_ne!(folded, text),
            }
            if text.is_ascii() {
                assert!(matches!(fold(&text), Cow::Borrowed(_)), "{text:?}");
            }
        }
    }
}
