//! The `edit` fallback for an `old_string` that is right but for whitespace.
//!
//! Models copy code with trailing spaces dropped, indentation shifted or
//! spaces where the file has tabs, and typographic quotes where it has ASCII.
//! An exact match then fails and the model has to read the file again and
//! retry. When the text fits exactly one place once each line is compared
//! without its surrounding whitespace, that place is the one meant, so it is
//! edited, and the new lines are re-indented the way the matched lines
//! differed. Same idea as omp's fuzzy edit match (MIT), restricted to
//! whitespace and punctuation: nothing that changes a token is ever ignored,
//! and two places that fit are never chosen between.

use std::ops::Range;

/// What a loose match of `old` in `text` came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Loose {
    /// No place fits, even loosely.
    Nowhere,
    /// This many places fit; choosing one would be a guess.
    Ambiguous(usize),
    /// The one place that fits, with `new` in it.
    Replaced(String),
}

/// Replaces the one run of whole lines in `text` that `old` matches line by
/// line once whitespace and typographic punctuation are set aside.
pub(crate) fn replace(text: &str, old: &str, new: &str) -> Loose {
    // A trailing newline ends the last line rather than adding an empty one;
    // the replaced span stops before the matched lines' own last newline.
    let (old, new) = match old.strip_suffix('\n') {
        Some(old) => (old, new.strip_suffix('\n').unwrap_or(new)),
        None => (old, new),
    };
    let wanted: Vec<&str> = old.split('\n').collect();
    if wanted.iter().all(|line| line.trim().is_empty()) {
        return Loose::Nowhere;
    }
    let keys: Vec<String> = wanted.iter().map(|line| key(line)).collect();
    let lines = lines_with_spans(text);
    let fits: Vec<usize> = (0..lines.len().saturating_sub(keys.len() - 1))
        .filter(|&start| {
            keys.iter()
                .zip(&lines[start..])
                .all(|(key_wanted, (line, _))| *key_wanted == key(line))
        })
        .collect();
    let start = match fits.as_slice() {
        [] => return Loose::Nowhere,
        [start] => *start,
        many => return Loose::Ambiguous(many.len()),
    };
    let matched = &lines[start..start + keys.len()];
    // How each line's indentation was written in the call, and how it is
    // written in the file.
    let indents: Vec<(&str, &str)> = wanted
        .iter()
        .zip(matched)
        .filter(|(line, _)| !line.trim().is_empty())
        .map(|(line, (found, _))| (indent(line), indent(found)))
        .collect();
    let replacement: Vec<String> = new
        .split('\n')
        .map(|line| reindent(line, &indents))
        .collect();
    let span = matched[0].1.start..matched[matched.len() - 1].1.end;
    Loose::Replaced(format!(
        "{}{}{}",
        &text[..span.start],
        replacement.join("\n"),
        &text[span.end..]
    ))
}

/// Each line of `text` and its byte span, newline excluded.
fn lines_with_spans(text: &str) -> Vec<(&str, Range<usize>)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        lines.push((line, start..start + line.len()));
        start += line.len() + 1;
    }
    lines
}

/// What a line is compared by: its text without the whitespace around it,
/// with runs of whitespace inside it made one space, and with the typographic
/// quotes, dashes and spaces a model writes in place of ASCII made ASCII.
fn key(line: &str) -> String {
    let mut key = String::with_capacity(line.len());
    for ch in line.chars() {
        let ch = match ch {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}'..='\u{201b}' => '\'',
            '\u{201c}'..='\u{201f}' => '"',
            '\u{200b}'..='\u{200d}' | '\u{feff}' => continue,
            // No-break and other typographic spaces are whitespace too.
            ch if ch.is_whitespace() => ' ',
            ch => ch,
        };
        if ch != ' ' {
            key.push(ch);
        } else if !key.is_empty() && !key.ends_with(' ') {
            key.push(' ');
        }
    }
    key.truncate(key.trim_end().len());
    key
}

fn indent(line: &str) -> &str {
    let text = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - text.len()]
}

/// `line` with its indentation written the file's way: the call's indent it
/// starts with — the longest one the matched lines used — swapped for the
/// file's. A line deeper than any matched one keeps the extra as written.
fn reindent(line: &str, indents: &[(&str, &str)]) -> String {
    if line.trim().is_empty() {
        return String::new();
    }
    let own = indent(line);
    match indents
        .iter()
        .filter(|(called, _)| own.starts_with(called))
        .max_by_key(|(called, _)| called.len())
    {
        Some((called, file)) => format!("{file}{}", &line[called.len()..]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_whitespace_and_typographic_punctuation_only() {
        assert_eq!(key("  let  x =\t1;   "), "let x = 1;");
        assert_eq!(
            key("\u{201c}a\u{201d} \u{2014} \u{2018}b\u{2019}"),
            "\"a\" - 'b'"
        );
        assert_ne!(key("let x = 1;"), key("let x = 2;"));
        assert_ne!(key("a.b"), key("a. b"));
    }

    #[test]
    fn a_blank_old_string_fits_nowhere() {
        assert_eq!(replace("a\n\nb\n", "\n  \n", "x"), Loose::Nowhere);
    }
}
