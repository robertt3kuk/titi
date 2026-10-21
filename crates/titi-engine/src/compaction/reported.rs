//! What the provider said a request weighed.
//!
//! `len / 4` is a guess that ignores the tool declarations and runs low for
//! code; the prompt-token count a provider reports for the request it just
//! served is the real figure, and the one its own context-length check uses.
//! So the compaction trigger prefers it: the count of the last request plus an
//! estimate of only what was appended since.

use std::sync::{Arc, Mutex, PoisonError};

use titi_providers::ChatMessage;

use super::estimate_request;

/// The provider's prompt-token count for a request, and how many messages
/// that request held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reported {
    pub prompt_tokens: u64,
    pub messages: usize,
}

impl Reported {
    /// `None` for a count of zero: a provider that measured nothing.
    pub fn new(prompt_tokens: u64, messages: usize) -> Option<Self> {
        (prompt_tokens > 0).then_some(Self {
            prompt_tokens,
            messages,
        })
    }
}

/// The tokens the request holds: the provider's count for the prefix it saw
/// plus an estimate of what came after, or an estimate of the whole thing
/// when there is no usable count. A count of zero is a provider that did not
/// measure, and one longer than the request describes a request that has
/// since been rewritten.
pub fn context_used(messages: &[ChatMessage], reported: Option<Reported>) -> u64 {
    match reported {
        Some(seen) if seen.prompt_tokens > 0 && seen.messages <= messages.len() => seen
            .prompt_tokens
            .saturating_add(estimate_request(&messages[seen.messages..])),
        _ => estimate_request(messages),
    }
}

#[derive(Debug, Clone, Copy)]
struct Carried {
    prompt_tokens: u64,
    /// Messages of the history (no system prompt) the request held.
    at: usize,
    /// Length of the history when the turn ended: what the count is only
    /// valid against.
    history_len: usize,
}

/// The last request's count, kept between turns.
///
/// The first round of a turn starts from the history the last one left, so
/// without this the one moment a long history is most likely to cross the
/// threshold is judged by the estimate. A count only describes the history it
/// was taken from: [`recall`](Self::recall) answers nothing once the history
/// has changed length, and whoever rewrites it in place (a rewind, a manual
/// compaction) calls [`forget`](Self::forget).
#[derive(Clone, Default)]
pub struct PromptMemory(Arc<Mutex<Option<Carried>>>);

impl PromptMemory {
    /// The carried count as a [`Reported`] over a request that opens with
    /// `lead` system messages and then the `history_len` messages the count
    /// was taken from.
    pub fn recall(&self, history_len: usize, lead: usize) -> Option<Reported> {
        let carried = (*self.0.lock().unwrap_or_else(PoisonError::into_inner))?;
        (carried.history_len == history_len).then_some(Reported {
            prompt_tokens: carried.prompt_tokens,
            messages: lead + carried.at,
        })
    }

    /// Keeps what a finished turn last learned. `stripped` is how many leading
    /// messages the request held that the history does not (the system
    /// prompt); `history_len` is the history the turn hands back.
    pub fn remember(&self, reported: Option<Reported>, stripped: usize, history_len: usize) {
        let carried = reported.map(|seen| Carried {
            prompt_tokens: seen.prompt_tokens,
            at: seen.messages.saturating_sub(stripped).min(history_len),
            history_len,
        });
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = carried;
    }

    pub fn forget(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titi_providers::Role;

    fn message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_providers_count_stands_for_the_prefix_it_saw() {
        let messages = vec![
            message(Role::User, "1234"),
            message(Role::Assistant, "12345678"),
            message(Role::User, "123456789012"),
        ];
        // Two messages were seen and counted at 500; only the third is new.
        let seen = Reported {
            prompt_tokens: 500,
            messages: 2,
        };
        assert_eq!(context_used(&messages, Some(seen)), 500 + 3);
        assert_eq!(context_used(&messages, None), 1 + 2 + 3);
    }

    #[test]
    fn a_count_that_does_not_fit_the_request_is_ignored() {
        let messages = vec![message(Role::User, "1234")];
        let silent = Reported {
            prompt_tokens: 0,
            messages: 1,
        };
        let stale = Reported {
            prompt_tokens: 900,
            messages: 5,
        };
        assert_eq!(context_used(&messages, Some(silent)), 1);
        assert_eq!(context_used(&messages, Some(stale)), 1);
    }

    #[test]
    fn a_count_is_carried_only_to_the_history_it_describes() {
        let memory = PromptMemory::default();
        // The request held a system prompt and four history messages; the
        // turn then added a reply, so the history it hands back has five.
        memory.remember(
            Some(Reported {
                prompt_tokens: 700,
                messages: 5,
            }),
            1,
            5,
        );
        assert_eq!(
            memory.recall(5, 1),
            Some(Reported {
                prompt_tokens: 700,
                messages: 5
            }),
            "the next request opens with the same one system message"
        );
        assert_eq!(memory.recall(6, 1), None, "the history grew underneath it");
        memory.forget();
        assert_eq!(memory.recall(5, 1), None);
    }
}
