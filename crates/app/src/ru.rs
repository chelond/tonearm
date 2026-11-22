//! Russian number words: "1 трек", "2 трека", "5 треков", with a thin no-break space between
//! thousands ("1 284 трека"). The same rules exist in Slint as the `Ru` global (theme.slint)
//! for counts the UI formats itself.

/// The three forms of a noun after a number: for 1 (21, 101…), for 2–4 (22–24…), for the rest.
pub type Forms = [&'static str; 3];

pub const TRACKS: Forms = ["трек", "трека", "треков"];
pub const PLAYS: Forms = ["прослушивание", "прослушивания", "прослушиваний"];
pub const ARTISTS: Forms = ["исполнитель", "исполнителя", "исполнителей"];
pub const DAYS: Forms = ["день", "дня", "дней"];
pub const TIMES: Forms = ["раз", "раза", "раз"];

/// U+202F narrow no-break space: groups digits without letting the number break across lines.
const GROUP: char = '\u{202F}';

/// The form that goes after `n`.
#[must_use]
pub fn form(n: u64, forms: Forms) -> &'static str {
    match (n % 10, n % 100) {
        (_, 11..=14) => forms[2],
        (1, _) => forms[0],
        (2..=4, _) => forms[1],
        _ => forms[2],
    }
}

/// `1284` → `"1 284"` (narrow no-break space); numbers under 10 000 too, as in print.
#[must_use]
pub fn number(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(GROUP);
        }
        out.push(c);
    }
    out
}

/// `count(1284, TRACKS)` → `"1 284 трека"`.
#[must_use]
pub fn count(n: u64, forms: Forms) -> String {
    format!("{} {}", number(n), form(n, forms))
}

/// [`count`] for `usize` lengths.
#[must_use]
pub fn count_of(n: usize, forms: Forms) -> String {
    count(u64::try_from(n).unwrap_or(u64::MAX), forms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_follow_the_last_two_digits() {
        let cases = [
            (0, "треков"),
            (1, "трек"),
            (2, "трека"),
            (4, "трека"),
            (5, "треков"),
            (11, "треков"),
            (12, "треков"),
            (14, "треков"),
            (21, "трек"),
            (22, "трека"),
            (25, "треков"),
            (101, "трек"),
            (111, "треков"),
            (112, "треков"),
            (1284, "трека"),
            (1000, "треков"),
        ];
        for (n, want) in cases {
            assert_eq!(form(n, TRACKS), want, "{n}");
        }
        assert_eq!(form(3, TIMES), "раза");
        assert_eq!(form(5, TIMES), "раз");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(number(0), "0");
        assert_eq!(number(999), "999");
        assert_eq!(number(1284), "1\u{202F}284");
        assert_eq!(number(1_000_000), "1\u{202F}000\u{202F}000");
        assert_eq!(count(1284, TRACKS), "1\u{202F}284 трека");
        assert_eq!(count_of(74, TRACKS), "74 трека");
        assert_eq!(count_of(91, PLAYS), "91 прослушивание");
    }
}
