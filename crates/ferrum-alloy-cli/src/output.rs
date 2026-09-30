use std::ops::RangeInclusive;

/// Unicode format characters (general category `Cf`), and the line and
/// paragraph separators. They can reorder, hide, or join terminal text.
const FORMAT_CHARACTERS: &[RangeInclusive<char>] = &[
    '\u{00AD}'..='\u{00AD}',
    '\u{0600}'..='\u{0605}',
    '\u{061C}'..='\u{061C}',
    '\u{06DD}'..='\u{06DD}',
    '\u{070F}'..='\u{070F}',
    '\u{0890}'..='\u{0891}',
    '\u{08E2}'..='\u{08E2}',
    '\u{180E}'..='\u{180E}',
    '\u{200B}'..='\u{200F}',
    '\u{2028}'..='\u{202E}',
    '\u{2060}'..='\u{206F}',
    '\u{FEFF}'..='\u{FEFF}',
    '\u{FFF9}'..='\u{FFFB}',
    '\u{110BD}'..='\u{110BD}',
    '\u{110CD}'..='\u{110CD}',
    '\u{13430}'..='\u{1343F}',
    '\u{1BCA0}'..='\u{1BCA3}',
    '\u{1D173}'..='\u{1D17A}',
    '\u{E0001}'..='\u{E0001}',
    '\u{E0020}'..='\u{E007F}',
];

/// Replaces control and format characters other than newlines before text is
/// shown in a terminal. User supplied values cannot control or disguise it.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => c,
            c if c.is_control() => '?',
            c if FORMAT_CHARACTERS.iter().any(|range| range.contains(&c)) => '?',
            c => c,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::printable;

    #[test]
    fn printable_replaces_control_and_format_characters() {
        let bidi = "a\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}\u{2066}\u{2067}\u{2068}\u{2069}b";
        assert_eq!(printable(bidi), "a?????????b");
        let hidden = "\u{200B}\u{200D}\u{200E}\u{200F}\u{061C}\u{FEFF}\u{00AD}\u{E0041}";
        assert_eq!(printable(hidden), "????????");
        assert_eq!(printable("\u{2028}\u{2029}"), "??");
        assert_eq!(printable("\u{1b}[31mred\u{7}\r\u{9b}"), "?[31mred???");
        let kept = "route /orders/{id}\n\tstatus 503 · 12.5 ms, café 東京 ✓\n";
        assert_eq!(printable(kept), kept.replace('\t', "?"));
    }
}
