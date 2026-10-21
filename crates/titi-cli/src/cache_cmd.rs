//! `/cache`: what the provider's prompt cache did for the last requests, and
//! what had changed in each request since the one before.
//!
//! The engine reports one [`EngineEvent::CacheObserved`] per request the
//! provider sized; the screen keeps the latest few and prints them on
//! demand. Nothing here ever sees prompt text: the reasons are the engine's
//! own phrases of names and counts.

use std::collections::VecDeque;

use titi_engine::TurnId;

use crate::chat::{Applied, Chat};
use crate::transcript::LineKind;

/// Requests kept for `/cache`.
pub(crate) const KEPT: usize = 50;

/// Rows `/cache` prints when it is not told how many.
const DEFAULT_ROWS: usize = 10;

/// One request's cache report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheRow {
    pub(crate) turn: u64,
    pub(crate) prompt: u32,
    pub(crate) cached: u32,
    pub(crate) change: String,
    pub(crate) missed: bool,
}

/// What the screen holds for `/cache`.
#[derive(Debug, Default)]
pub(crate) struct CacheLog {
    rows: VecDeque<CacheRow>,
    /// Requests seen, including the ones that have scrolled out of `rows`.
    seen: u64,
}

impl CacheLog {
    pub(crate) fn record(&mut self, row: CacheRow) {
        self.seen += 1;
        if self.rows.len() == KEPT {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }
}

/// `12.1k` for 12,100, `850` for 850.
fn tokens(count: u32) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", f64::from(count) / 1000.0)
    }
}

fn percent(cached: u32, prompt: u32) -> u32 {
    if prompt == 0 {
        0
    } else {
        (u64::from(cached) * 100 / u64::from(prompt)) as u32
    }
}

/// The lines `/cache` prints for the newest `limit` rows of `log`.
pub(crate) fn cache_lines(log: &CacheLog, limit: usize) -> Vec<String> {
    if log.rows.is_empty() {
        return vec![
            "cache: no request has reported its cache yet (a provider that does not report cached \
             tokens never will)"
                .to_owned(),
        ];
    }
    let prompt: u64 = log.rows.iter().map(|row| u64::from(row.prompt)).sum();
    let cached: u64 = log.rows.iter().map(|row| u64::from(row.cached)).sum();
    let misses = log.rows.iter().filter(|row| row.missed).count();
    let mut lines = vec![format!(
        "cache · {} requests · {}% from cache · {misses} {}",
        log.rows.len(),
        (cached * 100).checked_div(prompt).unwrap_or(0),
        if misses == 1 { "miss" } else { "misses" },
    )];
    let shown = limit.min(log.rows.len());
    let first = log.seen - shown as u64 + 1;
    for (offset, row) in log.rows.iter().skip(log.rows.len() - shown).enumerate() {
        let mark = if row.missed { "miss" } else { "ok  " };
        lines.push(format!(
            "  #{:<3} turn {:<3} {:>3}% · {} of {} · {mark} · {}",
            first + offset as u64,
            row.turn,
            percent(row.cached, row.prompt),
            tokens(row.cached),
            tokens(row.prompt),
            row.change,
        ));
    }
    lines
}

impl Chat {
    /// An engine report, for `/cache`.
    pub(crate) fn record_cache(
        &mut self,
        turn_id: TurnId,
        prompt: u32,
        cached: u32,
        change: &str,
        missed: bool,
    ) {
        self.cache_log.record(CacheRow {
            turn: turn_id.0,
            prompt,
            cached,
            change: change.to_owned(),
            missed,
        });
    }

    /// `/cache [n]`: the last `n` requests (ten by default) and why any
    /// of them missed.
    pub(crate) fn cache_command(&mut self, args: &str) -> Applied {
        let limit = match args.trim() {
            "" => DEFAULT_ROWS,
            number => match number.parse::<usize>() {
                Ok(count) if count > 0 => count.min(KEPT),
                _ => {
                    self.push(LineKind::Note, format!("usage: /cache [n] (got {args})"));
                    return Applied::none();
                }
            },
        };
        for line in cache_lines(&self.cache_log, limit) {
            self.push(LineKind::Note, line);
        }
        Applied::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(turn: u64, prompt: u32, cached: u32, change: &str, missed: bool) -> CacheRow {
        CacheRow {
            turn,
            prompt,
            cached,
            change: change.to_owned(),
            missed,
        }
    }

    #[test]
    fn an_empty_log_says_why() {
        let lines = cache_lines(&CacheLog::default(), 10);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("cache: no request has reported"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_newest_rows_are_listed_with_share_size_and_reason() {
        let mut log = CacheLog::default();
        log.record(row(1, 5000, 0, "first request", true));
        log.record(row(2, 5100, 4900, "prefix unchanged", false));
        log.record(row(3, 5200, 60, "tool list changed (+web)", true));
        let lines = cache_lines(&log, 10);
        // (0 + 4900 + 60) of 15,300 prompt tokens.
        assert_eq!(lines[0], "cache · 3 requests · 32% from cache · 2 misses");
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[1],
            "  #1   turn 1     0% · 0 of 5.0k · miss · first request"
        );
        assert_eq!(
            lines[2],
            "  #2   turn 2    96% · 4.9k of 5.1k · ok   · prefix unchanged"
        );
        assert_eq!(
            lines[3],
            "  #3   turn 3     1% · 60 of 5.2k · miss · tool list changed (+web)"
        );
    }

    #[test]
    fn a_limit_keeps_the_newest_and_numbers_them_by_request() {
        let mut log = CacheLog::default();
        for turn in 1..=5 {
            log.record(row(turn, 2000, 1900, "prefix unchanged", false));
        }
        let lines = cache_lines(&log, 2);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[1].starts_with("  #4 "), "{lines:?}");
        assert!(lines[2].starts_with("  #5 "), "{lines:?}");
        assert!(lines[0].contains("5 requests"), "{lines:?}");
    }

    #[test]
    fn the_log_keeps_only_the_latest_and_still_counts_the_rest() {
        let mut log = CacheLog::default();
        for turn in 1..=(KEPT as u64 + 5) {
            log.record(row(turn, 2000, 1900, "prefix unchanged", false));
        }
        let lines = cache_lines(&log, 1);
        assert!(lines[0].contains(&format!("{KEPT} requests")), "{lines:?}");
        assert!(
            lines[1].starts_with(&format!("  #{} ", KEPT + 5)),
            "{lines:?}"
        );
    }

    fn chat() -> Chat {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        let theme = titi_tui::theme::loader::load_theme("titanium", &options)
            .unwrap_or_else(|reason| panic!("built-in theme: {reason}"));
        Chat::new("openai/gpt-4.1", "session-1", std::sync::Arc::new(theme))
    }

    fn said(chat: &Chat) -> Vec<String> {
        chat.lines.iter().map(|line| line.text.clone()).collect()
    }

    /// The engine's reports reach `/cache`; it lists them, takes a count and
    /// refuses anything else.
    #[test]
    fn the_engines_reports_reach_the_slash_command() {
        use titi_engine::EngineEvent;
        let mut chat = chat();
        chat.cache_command("");
        assert!(said(&chat)[0].starts_with("cache: no request has reported"));

        for (prompt, cached, change, missed) in [
            (5000, 0, "first request", true),
            (5100, 4900, "prefix unchanged", false),
            (5200, 40, "message 3 edited", true),
        ] {
            chat.on_event(EngineEvent::CacheObserved {
                turn_id: TurnId(1),
                prompt_tokens: prompt,
                cached_tokens: cached,
                change: change.into(),
                missed,
            });
        }
        let before = said(&chat).len();
        chat.cache_command("2");
        let lines = &said(&chat)[before..];
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("3 requests"), "{lines:?}");
        assert!(lines[2].ends_with("miss · message 3 edited"), "{lines:?}");

        let before = said(&chat).len();
        chat.cache_command("lots");
        assert_eq!(said(&chat)[before], "usage: /cache [n] (got lots)");
    }
}
