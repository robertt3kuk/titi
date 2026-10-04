//! Terminal output as the text a person would read off the screen.
//!
//! A command run on a pty — and plenty run on a pipe: `curl`, `pip`, a
//! forced-colour test runner — writes for a terminal: colour codes, window
//! titles, links, and progress bars that redraw one line over and over with
//! `\r`. A model reading that raw pays for every redraw and every escape,
//! and a screen previewing it can be scrambled by a stray one. [`plain`]
//! plays the output the way a terminal would, line by line, and keeps only
//! the text that would be left standing. omp runs a native minimizer for
//! the same reason; this is the part of it that needs no knowledge of the
//! command that produced the output.

/// `raw` with escape sequences removed and each line's carriage returns,
/// backspaces and line erases applied, as a terminal would show it. Other
/// control characters are dropped; text, tabs and newlines pass untouched.
pub fn plain(raw: &str) -> String {
    if !raw
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        return raw.to_owned();
    }
    let mut screen = Screen::default();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => screen.newline(),
            '\r' => screen.cursor = 0,
            '\u{8}' => screen.cursor = screen.cursor.saturating_sub(1),
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    // Parameters and intermediates, then one final byte;
                    // a sequence cut off by the end of the output is dropped.
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            screen.control(next, &params);
                            break;
                        }
                        params.push(next);
                    }
                }
                // OSC, DCS, APC, PM and SOS run to BEL or ST (`ESC \`).
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // A charset designation names one more character.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\t' => screen.write(ch),
            ch if ch.is_control() => {}
            ch => screen.write(ch),
        }
    }
    screen.finish()
}

#[derive(Default)]
struct Screen {
    done: String,
    line: Vec<char>,
    cursor: usize,
}

impl Screen {
    fn write(&mut self, ch: char) {
        if self.cursor < self.line.len() {
            self.line[self.cursor] = ch;
        } else {
            self.line.resize(self.cursor, ' ');
            self.line.push(ch);
        }
        self.cursor += 1;
    }

    fn newline(&mut self) {
        self.done.extend(self.line.drain(..));
        self.done.push('\n');
        self.cursor = 0;
    }

    /// The CSI controls that change what a line holds: erase in line (`K`)
    /// and cursor to column (`G`). Colour, styles and every other control
    /// leave the text as it is.
    fn control(&mut self, last: char, params: &str) {
        match (last, params) {
            ('K', "" | "0") => self.line.truncate(self.cursor),
            ('K', "1") => {
                let end = self.cursor.min(self.line.len());
                self.line[..end].fill(' ');
            }
            ('K', "2") => self.line.clear(),
            ('G', column) => self.cursor = column.parse::<usize>().unwrap_or(1).saturating_sub(1),
            _ => {}
        }
    }

    fn finish(mut self) -> String {
        self.done.extend(self.line);
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_styles_are_dropped() {
        assert_eq!(
            plain("\u{1b}[1;31merror\u{1b}[0m: \u{1b}[38;2;0;180;255mbad\u{1b}[m\n"),
            "error: bad\n"
        );
    }

    #[test]
    fn a_carriage_return_overwrites_the_line() {
        // A progress bar redraws one line; only its last state is text.
        assert_eq!(
            plain("Downloading  10%\rDownloading  55%\rDownloading 100%\ndone\n"),
            "Downloading 100%\ndone\n"
        );
        // A shorter rewrite leaves the tail it did not cover, as on screen…
        assert_eq!(plain("abcdef\rXY\n"), "XYcdef\n");
        // …unless the line is erased first, as every progress bar does.
        assert_eq!(plain("downloading 100%\r\u{1b}[Kdone\n"), "done\n");
        assert_eq!(plain("downloading 100%\r\u{1b}[2Kdone\n"), "done\n");
        // A CR that only ends a line, CRLF style, changes nothing.
        assert_eq!(plain("one\r\ntwo\r\n"), "one\ntwo\n");
    }

    #[test]
    fn a_backspace_steps_back_over_the_line() {
        assert_eq!(plain("spin |\u{8}/\u{8}-\u{8}\\\u{8}ok\n"), "spin ok\n");
    }

    #[test]
    fn titles_links_and_charset_switches_leave_their_text() {
        assert_eq!(
            plain(
                "\u{1b}]0;window title\u{7}\u{1b}]8;;https://example.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\ \u{1b}(Bok\n"
            ),
            "link ok\n"
        );
    }

    #[test]
    fn plain_text_and_wide_text_pass_untouched() {
        let text = "src/lib.rs:12:fn main() {}\n日本語 🎉\ttab\n";
        assert_eq!(plain(text), text);
        assert_eq!(plain(""), "");
        // An escape cut off at the end of a capture is dropped, not kept raw.
        assert_eq!(plain("half \u{1b}[3"), "half ");
    }
}
