pub(crate) fn song_verse_label(index: usize) -> String {
    if index == 0 {
        "0".to_string()
    } else {
        format!("-{index}")
    }
}

/// Split a verse into display lines: `(line, stanza break after it)`.
///
/// The way to write a verse is one line of verse per line of text, with a
/// blank line between stanzas. A blank line before the first line or after
/// the last one marks nothing, and several blank lines are one break.
///
/// Older verses mark their lines with `/` instead, and both forms are still
/// read. Such a verse marks its lines with `/`, almost always mid-line. In the 285
/// verses and counterpoints of default_x1 (2026-10-02) the marks are written
/// "ends. / Next" (1213), "ends. /Next" (435) and "ends./Next" (838), and
/// never "ends/ next". So a `/` is a line mark when whitespace comes before
/// it, or when it follows closing punctuation. Any other `/` is text:
/// "and/or", "try/except", "w/ friends", URLs. A `/` that ends a
/// newline-separated line also marks a stanza break after it.
pub(crate) fn parse_song_lines(text: &str) -> Vec<(String, bool)> {
    let mut lines: Vec<(String, bool)> = Vec::new();
    let mut blank_since_last_line = false;
    for raw in text.lines().map(str::trim) {
        if raw.is_empty() {
            blank_since_last_line = true;
            continue;
        }
        if std::mem::take(&mut blank_since_last_line) {
            if let Some(last) = lines.last_mut() {
                last.1 = true;
            }
        }
        let (body, has_break) = match raw.strip_suffix('/') {
            Some(stripped) => (stripped.trim_end(), true),
            None => (raw, false),
        };
        let segments = split_on_line_marks(body);
        let last = segments.len().saturating_sub(1);
        for (index, segment) in segments.into_iter().enumerate() {
            lines.push((segment, has_break && index == last));
        }
    }
    lines
}

/// Punctuation that ends a line just before a `/` mark ("sincere./We").
const LINE_END_PUNCTUATION: &[char] = &[
    '.', '!', '?', ';', ':', ',', '\u{2014}', '\u{2026}', '"', '\u{201D}', '\u{2019}', '\'', ')',
];

/// Whether the `/` at `index` marks a line rather than being text.
fn is_line_mark(chars: &[char], index: usize) -> bool {
    let Some(&before) = index.checked_sub(1).and_then(|at| chars.get(at)) else {
        return true;
    };
    if before.is_whitespace() {
        return true;
    }
    // "://" in a URL: neither slash marks a line.
    let after = chars.get(index + 1).copied();
    LINE_END_PUNCTUATION.contains(&before) && after != Some('/') && before != ':'
}

/// Split at each `/` that marks a line.
fn split_on_line_marks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '/' && is_line_mark(&chars, index) {
            segments.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    segments.push(current);
    segments
        .into_iter()
        .map(|segment| segment.trim().to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_lines_preserve_explicit_break_markers() {
        assert_eq!(
            parse_song_lines("first /\n\nsecond\nthird/"),
            vec![
                ("first".to_string(), true),
                ("second".to_string(), false),
                ("third".to_string(), true),
            ]
        );
    }

    fn lines(text: &str) -> Vec<String> {
        parse_song_lines(text).into_iter().map(|(line, _)| line).collect()
    }

    #[test]
    fn song_lines_split_on_mid_line_marks_in_every_spacing() {
        assert_eq!(
            lines("One ends. / Two ends. /Three ends./Four \u{2014}/Five"),
            vec!["One ends.", "Two ends.", "Three ends.", "Four \u{2014}", "Five"]
        );
    }

    #[test]
    fn song_lines_keep_slashes_inside_words_and_urls() {
        assert_eq!(
            lines("and/or try/except w/ friends https://example.com/a https://x.org/ next / last"),
            vec!["and/or try/except w/ friends https://example.com/a https://x.org/ next", "last"]
        );
    }

    #[test]
    fn song_lines_take_a_blank_line_as_a_stanza_break() {
        assert_eq!(
            parse_song_lines("\n\none\ntwo\n\n\n  \nthree\nfour\n\n"),
            vec![
                ("one".to_string(), false),
                ("two".to_string(), true),
                ("three".to_string(), false),
                ("four".to_string(), false),
            ]
        );
        // Windows line endings read the same.
        assert_eq!(
            parse_song_lines("one\r\n\r\ntwo"),
            vec![("one".to_string(), true), ("two".to_string(), false)]
        );
    }

    #[test]
    fn song_lines_put_a_trailing_stanza_break_after_the_last_segment() {
        assert_eq!(
            parse_song_lines("a / b /\nc"),
            vec![
                ("a".to_string(), false),
                ("b".to_string(), true),
                ("c".to_string(), false),
            ]
        );
    }
}
