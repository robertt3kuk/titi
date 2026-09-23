//! Naming a session from its first message.
//!
//! A generated title is shown in the session list and in the status bar, so
//! the model's answer is treated as input, not as truth: control characters
//! are flattened, the text is cut to a few words and a fixed width, and an
//! answer with nothing printable left in it yields no title at all.

/// Words a generated title keeps.
const MAX_WORDS: usize = 3;
/// Characters a generated title keeps, so one long word cannot push the rest
/// of a status line off screen.
const MAX_CHARS: usize = 40;
/// Characters of the first user message the naming model is shown: a title
/// comes from the opening request, not from the essay pasted under it.
const MAX_SOURCE_CHARS: usize = 600;

/// Edge decoration a model wraps a bare title in.
fn decoration(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '*' | '#' | '.' | ',' | ':' | ';' | '-'
    ) || c.is_whitespace()
}

/// The instruction for the cheap model, carrying the session's first user
/// message.
pub fn naming_prompt(first_message: &str) -> String {
    let source: String = first_message
        .trim()
        .chars()
        .take(MAX_SOURCE_CHARS)
        .collect();
    format!(
        "Title this coding session in {MAX_WORDS} words or fewer. Answer with \
         the title alone: no quotes, no punctuation, no explanation.\n\n\
         First message:\n{source}"
    )
}

/// Turns a model's answer into a session title, or `None` when nothing
/// usable is left of it.
pub fn clean_title(answer: &str) -> Option<String> {
    let flattened: String = answer
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let joined = flattened
        .split_whitespace()
        .take(MAX_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    let title: String = joined
        .trim_matches(decoration)
        .chars()
        .take(MAX_CHARS)
        .collect();
    let title = title.trim_end_matches(decoration).to_owned();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrapped_multiline_answer_becomes_a_short_plain_title() {
        assert_eq!(
            clean_title("\"Fix  the\n  parser bug\"\n").as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(
            clean_title("**rename session**").as_deref(),
            Some("rename session")
        );
    }

    #[test]
    fn an_answer_with_nothing_printable_yields_no_title() {
        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   \n\t  "), None);
        assert_eq!(clean_title("\"\" ... ---"), None);
        assert_eq!(clean_title("\u{7}\u{1b}"), None);
    }

    #[test]
    fn one_endless_word_is_capped_to_a_renderable_width() {
        let title = clean_title(&"x".repeat(500)).unwrap_or_else(|| panic!("a title"));
        assert_eq!(title.chars().count(), MAX_CHARS);
    }

    #[test]
    fn the_prompt_carries_a_bounded_slice_of_the_first_message() {
        let prompt = naming_prompt(&"война и мир ".repeat(500));
        assert!(prompt.contains("война и мир"), "{prompt}");
        assert!(
            prompt.chars().count() < MAX_SOURCE_CHARS + 200,
            "unbounded prompt: {} chars",
            prompt.chars().count()
        );
    }
}
