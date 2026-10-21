//! Why a request missed the provider's prompt cache.
//!
//! A provider caches the front of a request: its tools, then its system
//! prompt, then the messages in order, under one model and one reasoning
//! setting. Anything that changes a byte in that front makes everything after
//! it a cache miss, and the provider says so only as a smaller cached-token
//! count. This module remembers a fingerprint of each request — a hash of the
//! model and effort, of every tool spec in order, of each system block and of
//! each message — and compares it with the previous request of the session, so
//! a miss can be named: what changed first, in the order the provider reads.
//!
//! Only hashes and lengths are kept, and only names a person already sees
//! (tool names) and counts are ever said. No prompt text, tool description or
//! message content reaches a notice or an event.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use smol_str::SmolStr;
use titi_providers::{ChatMessage, Effort, Role, ToolSpec};

/// Below this a provider does not cache at all (the smallest minimum any of
/// them has), so a short request that read nothing is not a miss.
pub const MIN_CACHEABLE_TOKENS: u64 = 1024;

/// Tool names a notice lists before it says how many more there are.
const NAMES_SHOWN: usize = 3;

fn hash_of(text: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Block {
    hash: u64,
    chars: usize,
}

/// What a provider's cache keys on for one request, reduced to hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    model: SmolStr,
    effort: Option<Effort>,
    tools: Vec<(SmolStr, u64)>,
    system: Vec<Block>,
    messages: Vec<u64>,
}

impl Fingerprint {
    /// The fingerprint of a request to `model` with `effort` applied, with
    /// these tools and these messages. Leading system messages are the
    /// system prompt, cut into blocks at every `# ` heading line; the rest are
    /// the history.
    pub fn of(
        model: &str,
        effort: Option<Effort>,
        tools: &[ToolSpec],
        messages: &[ChatMessage],
    ) -> Self {
        let lead = messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .count();
        let mut system = Vec::new();
        for message in &messages[..lead] {
            system.extend(blocks(&message.content));
        }
        Self {
            model: model.into(),
            effort,
            tools: tools
                .iter()
                .map(|spec| (spec.name.clone(), hash_of(format!("{spec:?}"))))
                .collect(),
            system,
            messages: messages[lead..]
                .iter()
                .map(|message| hash_of(format!("{message:?}")))
                .collect(),
        }
    }
}

/// `text` cut into blocks where a line starts with `# `.
fn blocks(text: &str) -> Vec<Block> {
    let mut found: Vec<String> = Vec::new();
    for line in text.split_inclusive('\n') {
        match found.last_mut() {
            Some(current) if !line.starts_with("# ") => current.push_str(line),
            _ => found.push(line.to_owned()),
        }
    }
    found
        .into_iter()
        .map(|block| Block {
            hash: hash_of(&block),
            chars: block.chars().count(),
        })
        .collect()
}

/// The first thing that differs between two requests, in the order a
/// provider reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// There was no request before this one.
    First,
    /// A different model, or a different reasoning effort for it.
    Model,
    Tools {
        added: Vec<SmolStr>,
        removed: Vec<SmolStr>,
        changed: Vec<SmolStr>,
        /// The same tools in another order.
        reordered: bool,
    },
    System {
        /// 1-based, in the order the blocks appear.
        block: usize,
        /// Characters gained (positive) or lost.
        delta: i64,
    },
    /// A message the earlier request already sent is different now; 1-based
    /// among the messages after the system prompt.
    Message { at: usize },
    /// The history is shorter than the earlier request's, and the same as far
    /// as it goes.
    Rewound { dropped: usize },
    /// The earlier request is a prefix of this one.
    Unchanged,
}

impl Change {
    /// A sentence fragment with names and counts only.
    pub fn describe(&self) -> SmolStr {
        match self {
            Change::First => "first request".into(),
            Change::Model => "model or effort switched".into(),
            Change::Tools {
                added,
                removed,
                changed,
                reordered,
            } => {
                let mut parts = Vec::new();
                for (mark, names) in [('+', added), ('−', removed), ('~', changed)] {
                    for name in names.iter().take(NAMES_SHOWN) {
                        parts.push(format!("{mark}{name}"));
                    }
                    if names.len() > NAMES_SHOWN {
                        parts.push(format!("{mark}{} more", names.len() - NAMES_SHOWN));
                    }
                }
                if parts.is_empty() && *reordered {
                    "tool list reordered".into()
                } else {
                    format!("tool list changed ({})", parts.join(" ")).into()
                }
            }
            Change::System { block, delta } => {
                format!("system prompt changed (block {block}, {delta:+} chars)").into()
            }
            Change::Message { at } => format!("message {at} edited").into(),
            Change::Rewound { dropped } => {
                format!("history rewound ({dropped} messages dropped)").into()
            }
            Change::Unchanged => "prefix unchanged".into(),
        }
    }
}

/// What differs first between `previous` and `current`.
pub fn diff(previous: &Fingerprint, current: &Fingerprint) -> Change {
    if previous.model != current.model || previous.effort != current.effort {
        return Change::Model;
    }
    if previous.tools != current.tools {
        let names = |side: &Fingerprint| -> Vec<SmolStr> {
            side.tools.iter().map(|(name, _)| name.clone()).collect()
        };
        let (before, after) = (names(previous), names(current));
        let added = after
            .iter()
            .filter(|name| !before.contains(name))
            .cloned()
            .collect();
        let removed = before
            .iter()
            .filter(|name| !after.contains(name))
            .cloned()
            .collect();
        let changed = current
            .tools
            .iter()
            .filter(|(name, hash)| {
                previous
                    .tools
                    .iter()
                    .any(|(old, old_hash)| old == name && old_hash != hash)
            })
            .map(|(name, _)| name.clone())
            .collect();
        let mut sorted_before = before.clone();
        let mut sorted_after = after.clone();
        sorted_before.sort();
        sorted_after.sort();
        return Change::Tools {
            added,
            removed,
            changed,
            reordered: sorted_before == sorted_after && before != after,
        };
    }
    if previous.system != current.system {
        let at = previous
            .system
            .iter()
            .zip(&current.system)
            .position(|(old, new)| old != new)
            .unwrap_or_else(|| previous.system.len().min(current.system.len()));
        let chars = |side: &Fingerprint| side.system.get(at).map_or(0, |block| block.chars);
        return Change::System {
            block: at + 1,
            delta: chars(current) as i64 - chars(previous) as i64,
        };
    }
    if let Some(at) = previous
        .messages
        .iter()
        .zip(&current.messages)
        .position(|(old, new)| old != new)
    {
        return Change::Message { at: at + 1 };
    }
    if current.messages.len() < previous.messages.len() {
        return Change::Rewound {
            dropped: previous.messages.len() - current.messages.len(),
        };
    }
    Change::Unchanged
}

/// What one request told the watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub change: Change,
    /// The provider read much less from its cache than the earlier request
    /// should have left there.
    pub missed: bool,
    /// What to tell the person, when this miss is worth a line.
    pub notice: Option<SmolStr>,
}

#[derive(Default)]
struct State {
    previous: Option<Fingerprint>,
    /// The earlier request's prompt tokens: what an unchanged prefix should
    /// have read back.
    previous_prompt: u64,
    /// The provider has read from its cache at least once this session, so a
    /// zero is a miss and not a provider that does not cache.
    cache_seen: bool,
}

/// The session's memory of its last request.
#[derive(Clone, Default)]
pub struct CacheWatch(Arc<Mutex<State>>);

impl CacheWatch {
    /// Compares `fingerprint` with the previous request's, judges the miss
    /// from the provider's `prompt` and `cached` token counts, and keeps
    /// `fingerprint` as the one to compare against next.
    ///
    /// A count of zero prompt tokens is a provider that measured nothing and
    /// is never a miss. A miss is `cached` under half of what the earlier
    /// request should have left: its whole prompt, when the prefix held. With
    /// the prefix unchanged a miss is only worth a line once the provider has
    /// been seen to cache, because a provider that never does would repeat it
    /// on every request.
    pub fn observe(&self, fingerprint: Fingerprint, prompt: u64, cached: u64) -> Observation {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let change = match &state.previous {
            None => Change::First,
            Some(previous) => diff(previous, &fingerprint),
        };
        let expected = state.previous_prompt.min(prompt);
        let missed = prompt > 0
            && match change {
                Change::First => cached == 0 && prompt >= MIN_CACHEABLE_TOKENS,
                _ => expected >= MIN_CACHEABLE_TOKENS && cached.saturating_mul(2) < expected,
            };
        let notice = missed
            .then(|| match &change {
                Change::Unchanged if state.cache_seen => Some(SmolStr::from(
                    "cache miss · prefix unchanged (the provider's cache expired or was evicted)",
                )),
                Change::Unchanged => None,
                other => Some(format!("cache miss · {}", other.describe()).into()),
            })
            .flatten();
        if cached > 0 {
            state.cache_seen = true;
        }
        if prompt > 0 {
            state.previous_prompt = prompt;
        }
        state.previous = Some(fingerprint);
        Observation {
            change,
            missed,
            notice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, about: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: about.into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            ..Default::default()
        }
    }

    struct Request {
        model: &'static str,
        effort: Option<Effort>,
        tools: Vec<ToolSpec>,
        messages: Vec<ChatMessage>,
    }

    impl Request {
        fn base() -> Self {
            Self {
                model: "m",
                effort: None,
                tools: vec![spec("bash", "runs"), spec("read", "reads")],
                messages: vec![
                    message(
                        Role::System,
                        "# Manual\nwork well\n# Skills\n- review\n# Project\nrules\n",
                    ),
                    message(Role::User, "one"),
                    message(Role::Assistant, "two"),
                ],
            }
        }

        fn print(&self) -> Fingerprint {
            Fingerprint::of(self.model, self.effort, &self.tools, &self.messages)
        }
    }

    fn change_of(edit: impl FnOnce(&mut Request)) -> Change {
        let before = Request::base();
        let mut after = Request::base();
        edit(&mut after);
        diff(&before.print(), &after.print())
    }

    #[test]
    fn each_reason_is_classified_and_described() {
        assert_eq!(change_of(|_| {}), Change::Unchanged);
        assert_eq!(
            change_of(|r| r.messages.push(message(Role::User, "three"))),
            Change::Unchanged,
            "appending is not a change"
        );

        assert_eq!(change_of(|r| r.model = "other"), Change::Model);
        assert_eq!(change_of(|r| r.effort = Some(Effort::High)), Change::Model);
        assert_eq!(Change::Model.describe(), "model or effort switched");

        let added = change_of(|r| r.tools.push(spec("zed", "new")));
        assert_eq!(added.describe(), "tool list changed (+zed)", "{added:?}");
        let removed = change_of(|r| {
            r.tools.remove(0);
        });
        assert_eq!(removed.describe(), "tool list changed (−bash)");
        let changed = change_of(|r| r.tools[1] = spec("read", "reads more"));
        assert_eq!(changed.describe(), "tool list changed (~read)");
        let both = change_of(|r| {
            r.tools.remove(0);
            r.tools.push(spec("zed", "new"));
            r.tools[0] = spec("read", "different");
        });
        assert_eq!(both.describe(), "tool list changed (+zed −bash ~read)");
        let reordered = change_of(|r| r.tools.reverse());
        assert_eq!(reordered.describe(), "tool list reordered");

        // Block 2 is the skills block; five characters longer.
        let system = change_of(|r| {
            r.messages[0] = message(
                Role::System,
                "# Manual\nwork well\n# Skills\n- review\n- more\n# Project\nrules\n",
            );
        });
        assert_eq!(system, Change::System { block: 2, delta: 7 });
        assert_eq!(
            system.describe(),
            "system prompt changed (block 2, +7 chars)"
        );
        let shorter = change_of(|r| {
            r.messages[0] = message(
                Role::System,
                "# Manual\nwork\n# Skills\n- review\n# Project\nrules\n",
            );
        });
        assert_eq!(
            shorter.describe(),
            "system prompt changed (block 1, -5 chars)"
        );
        let appended = change_of(|r| {
            r.messages[0] = message(
                Role::System,
                "# Manual\nwork well\n# Skills\n- review\n# Project\nrules\n# More\nx\n",
            );
        });
        assert_eq!(
            appended.describe(),
            "system prompt changed (block 4, +9 chars)"
        );

        let edited = change_of(|r| r.messages[1] = message(Role::User, "ONE"));
        assert_eq!(edited, Change::Message { at: 1 });
        assert_eq!(edited.describe(), "message 1 edited");
        // A tool call recorded on a message is part of what was sent.
        let call = change_of(|r| {
            r.messages[2].tool_calls.push(titi_providers::ToolCallRef {
                call_id: "c".into(),
                name: "read".into(),
                ..Default::default()
            });
        });
        assert_eq!(call, Change::Message { at: 2 });

        let rewound = change_of(|r| {
            r.messages.pop();
        });
        assert_eq!(rewound.describe(), "history rewound (1 messages dropped)");
        assert_eq!(Change::First.describe(), "first request");
    }

    #[test]
    fn what_changed_first_is_what_the_provider_reads_first() {
        // The tool list, the system prompt and a message all changed: the
        // tools come first in a provider's prefix.
        let change = change_of(|r| {
            r.tools.push(spec("zed", "new"));
            r.messages[0] = message(Role::System, "# Other\n");
            r.messages[1] = message(Role::User, "ONE");
        });
        assert!(matches!(change, Change::Tools { .. }), "{change:?}");
        // The model beats them all.
        let change = change_of(|r| {
            r.model = "other";
            r.tools.clear();
        });
        assert_eq!(change, Change::Model);
    }

    #[test]
    fn an_unchanged_request_that_read_its_cache_says_nothing() {
        let watch = CacheWatch::default();
        let first = watch.observe(Request::base().print(), 5000, 0);
        assert_eq!(first.change, Change::First);
        assert_eq!(first.notice.as_deref(), Some("cache miss · first request"));
        for _ in 0..3 {
            let next = watch.observe(Request::base().print(), 5100, 4900);
            assert_eq!(next.change, Change::Unchanged);
            assert!(!next.missed && next.notice.is_none(), "{next:?}");
        }
    }

    #[test]
    fn a_change_that_costs_the_cache_is_named_and_one_that_does_not_is_not() {
        let watch = CacheWatch::default();
        watch.observe(Request::base().print(), 5000, 4000);
        // Effort switched, and the provider read almost nothing back.
        let mut switched = Request::base();
        switched.effort = Some(Effort::High);
        let miss = watch.observe(switched.print(), 5000, 100);
        assert!(miss.missed);
        assert_eq!(
            miss.notice.as_deref(),
            Some("cache miss · model or effort switched")
        );
        // The same request again hits.
        let next = watch.observe(switched.print(), 5200, 5000);
        assert!(!next.missed && next.notice.is_none());
        // A message near the end was edited but the provider still read most
        // of the prefix: a change, not a miss, so no line.
        let mut edited = Request::base();
        edited.effort = Some(Effort::High);
        edited.messages[2] = message(Role::Assistant, "TWO");
        let kept = watch.observe(edited.print(), 5200, 4500);
        assert_eq!(kept.change, Change::Message { at: 2 });
        assert!(!kept.missed && kept.notice.is_none(), "{kept:?}");
    }

    #[test]
    fn a_miss_with_the_prefix_unchanged_is_said_only_for_a_provider_that_caches() {
        // A provider that has never read its cache: silence, every request.
        let silent = CacheWatch::default();
        silent.observe(Request::base().print(), 5000, 0);
        for _ in 0..3 {
            let next = silent.observe(Request::base().print(), 5000, 0);
            assert!(next.missed);
            assert!(next.notice.is_none(), "{next:?}");
        }
        // One that has: the loss is worth a line.
        let watch = CacheWatch::default();
        watch.observe(Request::base().print(), 5000, 0);
        watch.observe(Request::base().print(), 5000, 4800);
        let expired = watch.observe(Request::base().print(), 5000, 0);
        assert!(expired.missed);
        assert_eq!(
            expired.notice.as_deref(),
            Some("cache miss · prefix unchanged (the provider's cache expired or was evicted)")
        );
    }

    #[test]
    fn short_requests_and_unmeasured_ones_are_never_a_miss() {
        let watch = CacheWatch::default();
        let short = watch.observe(Request::base().print(), 300, 0);
        assert!(!short.missed && short.notice.is_none());
        let silent = watch.observe(Request::base().print(), 0, 0);
        assert!(!silent.missed);
        let mut other = Request::base();
        other.model = "other";
        let small = watch.observe(other.print(), 400, 0);
        assert!(!small.missed && small.notice.is_none(), "{small:?}");
    }

    /// The point of keeping only hashes: what was in the prompt cannot reach
    /// a line, whatever changed.
    #[test]
    fn no_notice_carries_content() {
        let watch = CacheWatch::default();
        let mut one = Request::base();
        one.messages[0] = message(Role::System, "# Manual\nSECRET-RULE\n");
        one.messages[1] = message(Role::User, "SECRET-PROMPT");
        one.tools.push(spec("web", "SECRET-DESCRIPTION"));
        watch.observe(one.print(), 5000, 4000);
        let mut two = Request::base();
        two.messages[0] = message(Role::System, "# Manual\nOTHER-SECRET\n");
        two.messages[1] = message(Role::User, "OTHER-PROMPT");
        two.tools.push(spec("web", "OTHER-DESCRIPTION"));
        let seen = watch.observe(two.print(), 5000, 0);
        let text = format!("{seen:?} {}", seen.change.describe());
        assert!(seen.notice.is_some());
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("PROMPT"), "{text}");
        assert!(!text.contains("DESCRIPTION"), "{text}");
    }

    #[test]
    fn many_changed_tools_are_counted_not_listed() {
        let change = change_of(|r| {
            for name in ["a", "b", "c", "d", "e"] {
                r.tools.push(spec(name, "x"));
            }
        });
        assert_eq!(change.describe(), "tool list changed (+a +b +c +2 more)");
    }
}
