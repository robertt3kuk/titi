//! Compat policies: model quirks are declared as endpoint-family metadata,
//! never as provider-name branches. Two-phase resolution mirrors omp:
//! build-time `ModelCompat` (catalog) → request-time [`resolve_compat`].

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::transport::ApiKind;

/// Canonical effort scale (`packages/catalog/effort.ts` analog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

pub const EFFORT_LADDER: [Effort; 6] = [
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

impl Effort {
    /// The word a user types and a settings file holds.
    pub const fn name(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The level a word names, in any case.
    pub fn from_name(word: &str) -> Option<Self> {
        EFFORT_LADDER
            .iter()
            .copied()
            .find(|effort| effort.name().eq_ignore_ascii_case(word))
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Thinking wire dialect per endpoint family.
///
/// A dialect is only sent by the family it belongs to: [`resolve_reasoning`]
/// returns nothing for `Zai`, `Qwen` and `OpenRouter`, which no wire builder
/// writes yet, rather than guessing a body for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    /// Chat Completions `reasoning_effort` (or omitted for dialects).
    OpenAi,
    /// zai: `thinking: {type: "enabled"}`.
    Zai,
    /// qwen: `enable_thinking: bool`.
    Qwen,
    /// openrouter: `reasoning: {enabled: false}`.
    OpenRouter,
    /// Responses: `reasoning: {effort}`.
    OpenAiResponses,
    /// Anthropic adaptive: `thinking: {type: "adaptive"}` + output effort.
    AnthropicAdaptive,
    /// Anthropic `output_config.effort` alone, for a model that takes effort
    /// but not adaptive thinking (Opus 4.5 thinks only with `budget_tokens`).
    AnthropicEffort,
    /// Gemini: `thinkingConfig: {thinkingLevel}`.
    GoogleLevel,
    /// Gemini 2.x: `thinkingConfig: {thinkingBudget}` in thought tokens.
    GoogleBudget,
}

/// Where the generation cap goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictMode {
    All,
    Mixed,
    None,
}

/// Build-time compat spec from the model catalog.
///
/// A model without one takes no effort setting and is sent none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCompat {
    /// The levels the model accepts. A request for any other level is moved
    /// to the nearest of these by [`clamp_effort`]; an empty list accepts
    /// none and sends nothing.
    pub efforts: Vec<Effort>,
    /// The dialect this model speaks; `None` is its family's own.
    pub thinking_format: Option<ThinkingFormat>,
    /// Wire token per effort (effortMap); keys are ladder-ordered.
    pub effort_map: Vec<(Effort, SmolStr)>,
    /// Thinking cannot be turned off for this model.
    pub requires_effort: bool,
    /// An explicit "off" must still be sent on the wire.
    pub suppress_when_off: bool,
}

impl Default for ModelCompat {
    fn default() -> Self {
        Self {
            efforts: EFFORT_LADDER.to_vec(),
            thinking_format: None,
            effort_map: EFFORT_LADDER
                .iter()
                .map(|e| (*e, format!("{e:?}").to_lowercase().into()))
                .collect(),
            requires_effort: false,
            suppress_when_off: false,
        }
    }
}

/// Request-time options.
#[derive(Debug, Clone, Default)]
pub struct RequestOpts {
    pub effort: Option<Effort>,
}

/// Fully resolved request-time policy.
#[derive(Debug, Clone)]
pub struct CompatPolicy {
    pub thinking_format: ThinkingFormat,
    pub max_tokens_field: MaxTokensField,
    pub strict_mode: StrictMode,
    /// Swapped in by pointer when thinking is active (no re-build).
    pub when_thinking: Option<Box<CompatPolicy>>,
}

/// Resolve the policy for a request. Family decides the defaults; the model
/// ladder clamps the requested effort.
pub fn resolve_compat(api: ApiKind, model: &ModelCompat, opts: &RequestOpts) -> CompatPolicy {
    let _ = model;
    let base = CompatPolicy {
        thinking_format: match api {
            ApiKind::OpenAiCompletions => ThinkingFormat::OpenAi,
            ApiKind::OpenAiResponses => ThinkingFormat::OpenAiResponses,
            ApiKind::AnthropicMessages => ThinkingFormat::AnthropicAdaptive,
            ApiKind::GeminiGenerateContent => ThinkingFormat::GoogleLevel,
        },
        max_tokens_field: match api {
            ApiKind::OpenAiCompletions => MaxTokensField::MaxTokens,
            ApiKind::OpenAiResponses => MaxTokensField::MaxCompletionTokens,
            ApiKind::AnthropicMessages => MaxTokensField::MaxTokens,
            ApiKind::GeminiGenerateContent => MaxTokensField::MaxTokens,
        },
        strict_mode: match api {
            ApiKind::OpenAiCompletions => StrictMode::All,
            _ => StrictMode::None,
        },
        when_thinking: None,
    };
    match opts.effort {
        // Thinking requested: the policy carries a pointer-swapped
        // thinking-specific overlay (no full rebuild).
        Some(_) => CompatPolicy {
            when_thinking: Some(Box::new(base)),
            ..CompatPolicy {
                thinking_format: ThinkingFormat::OpenAi,
                max_tokens_field: MaxTokensField::MaxTokens,
                strict_mode: StrictMode::All,
                when_thinking: None,
            }
        },
        None => base,
    }
}

/// Move a requested effort to the nearest level the model accepts (Hermes-style:
/// never 400 on `xhigh` against an endpoint that only knows low..high, and
/// never on `medium` against one that lists only `low` and `high`). A tie goes
/// to the lower level, the cheaper of the two. A model that lists no level
/// leaves the request as it was: [`resolve_reasoning`] is what refuses it.
pub fn clamp_effort(requested: Effort, model: &ModelCompat) -> Effort {
    if model.efforts.contains(&requested) {
        return requested;
    }
    let distance = |level: Effort| (level as i8 - requested as i8).unsigned_abs();
    model
        .efforts
        .iter()
        .copied()
        .min_by_key(|level| (distance(*level), *level))
        .unwrap_or(requested)
}

/// An effort resolved against one model and one family: what the wire builder
/// writes, and nothing it has to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reasoning {
    pub format: ThinkingFormat,
    /// The level actually sent, after the model's ladder moved the request.
    pub effort: Effort,
    pub value: ReasoningValue,
}

/// The value a dialect carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningValue {
    /// A named level: `"high"`.
    Token(SmolStr),
    /// Thought tokens, for [`ThinkingFormat::GoogleBudget`].
    Budget(u32),
}

/// Thought-token budget per level for a budget-dialect Gemini model. `high`
/// is the most a 2.5 Flash accepts (24 576), so a model declaring only levels
/// up to `high` is never sent a budget it rejects; 2.5 Pro goes further, and
/// declares `xhigh` and `max` to be sent its own ceiling (32 768).
const fn google_budget(effort: Effort) -> u32 {
    match effort {
        Effort::Minimal => 512,
        Effort::Low => 1_024,
        Effort::Medium => 8_192,
        Effort::High => 24_576,
        Effort::Xhigh | Effort::Max => 32_768,
    }
}

fn family_format(api: ApiKind) -> ThinkingFormat {
    match api {
        ApiKind::OpenAiCompletions => ThinkingFormat::OpenAi,
        ApiKind::OpenAiResponses => ThinkingFormat::OpenAiResponses,
        ApiKind::AnthropicMessages => ThinkingFormat::AnthropicAdaptive,
        ApiKind::GeminiGenerateContent => ThinkingFormat::GoogleLevel,
    }
}

/// What to send for the requested effort, or `None` when nothing may be:
/// no effort asked for, a model with no declared support (`model` is `None`
/// or lists no level), or a dialect the family's wire builder does not write.
///
/// "Nothing" is the safe answer in every one of those cases: a provider that
/// is sent a field it does not know answers 400, a provider that is sent none
/// answers with its own default.
pub fn resolve_reasoning(
    api: ApiKind,
    model: Option<&ModelCompat>,
    opts: &RequestOpts,
) -> Option<Reasoning> {
    let model = model?;
    let requested = opts.effort?;
    if model.efforts.is_empty() {
        return None;
    }
    let format = model
        .thinking_format
        .clone()
        .unwrap_or_else(|| family_format(api));
    let belongs = match api {
        ApiKind::OpenAiCompletions => format == ThinkingFormat::OpenAi,
        ApiKind::OpenAiResponses => format == ThinkingFormat::OpenAiResponses,
        ApiKind::AnthropicMessages => matches!(
            format,
            ThinkingFormat::AnthropicAdaptive | ThinkingFormat::AnthropicEffort
        ),
        ApiKind::GeminiGenerateContent => matches!(
            format,
            ThinkingFormat::GoogleLevel | ThinkingFormat::GoogleBudget
        ),
    };
    if !belongs {
        return None;
    }
    let effort = clamp_effort(requested, model);
    let value = if format == ThinkingFormat::GoogleBudget {
        ReasoningValue::Budget(google_budget(effort))
    } else {
        let token = model
            .effort_map
            .iter()
            .find(|(level, _)| *level == effort)
            .map_or_else(|| SmolStr::new(effort.name()), |(_, token)| token.clone());
        ReasoningValue::Token(token)
    };
    Some(Reasoning {
        format,
        effort,
        value,
    })
}

/// Decoder-side policy flags derived from compat (quirks are flags, not name
/// branches).
#[derive(Debug, Clone, Default)]
pub struct StreamDecodePolicy {
    /// Reasoning deltas are cumulative snapshots (MiniMax-class): dedupe by
    /// emitting only the suffix beyond the previous snapshot.
    pub reasoning_deltas_cumulative: bool,
    /// `delta.content` arrives as an array of text parts (Mistral-class).
    pub content_is_parts_array: bool,
}

impl StreamDecodePolicy {
    pub fn from_compat(_c: &CompatPolicy) -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(efforts: &[Effort]) -> ModelCompat {
        ModelCompat {
            efforts: efforts.to_vec(),
            ..ModelCompat::default()
        }
    }

    #[test]
    fn family_defaults() {
        let m = ModelCompat::default();
        let o = RequestOpts::default();
        let p = resolve_compat(ApiKind::OpenAiCompletions, &m, &o);
        assert_eq!(p.max_tokens_field, MaxTokensField::MaxTokens);
        assert_eq!(p.strict_mode, StrictMode::All);
        let p = resolve_compat(ApiKind::OpenAiResponses, &m, &o);
        assert_eq!(p.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        let p = resolve_compat(ApiKind::AnthropicMessages, &m, &o);
        assert!(matches!(
            p.thinking_format,
            ThinkingFormat::AnthropicAdaptive
        ));
        let p = resolve_compat(ApiKind::GeminiGenerateContent, &m, &o);
        assert!(matches!(p.thinking_format, ThinkingFormat::GoogleLevel));
    }

    #[test]
    fn effort_clamp_wire_table() {
        // Model supports only low..high (xAI Actual-class endpoint).
        let m = ladder(&[Effort::Low, Effort::Medium, Effort::High]);
        let cases = [
            (Effort::Minimal, Effort::Low),
            (Effort::Low, Effort::Low),
            (Effort::Medium, Effort::Medium),
            (Effort::High, Effort::High),
            (Effort::Xhigh, Effort::High),
            (Effort::Max, Effort::High),
        ];
        for (requested, expected) in cases {
            assert_eq!(
                clamp_effort(requested, &m),
                expected,
                "clamping {requested:?}"
            );
        }
        // Global xhigh must not 400 a low..max endpoint.
        assert_eq!(clamp_effort(Effort::Xhigh, &m), Effort::High);
    }

    #[test]
    fn effort_map_gives_wire_token() {
        let m = ModelCompat::default();
        let wire = |e: Effort| {
            m.effort_map
                .iter()
                .find(|(k, _)| *k == e)
                .map(|(_, v)| v.clone())
                .expect("ladder entry")
        };
        assert_eq!(wire(Effort::Minimal), "minimal");
        assert_eq!(wire(Effort::Xhigh), "xhigh");
        assert_eq!(wire(Effort::Max), "max");
    }

    #[test]
    fn policy_swap_on_thinking_is_pointer_level() {
        let m = ModelCompat::default();
        let mut o = RequestOpts::default();
        o.effort = Some(Effort::High);
        let p = resolve_compat(ApiKind::OpenAiCompletions, &m, &o);
        assert!(p.when_thinking.is_some());
        let o = RequestOpts::default();
        let p = resolve_compat(ApiKind::OpenAiCompletions, &m, &o);
        assert!(p.when_thinking.is_none());
    }

    #[test]
    fn effort_clamp_snaps_into_a_gap_toward_the_cheaper_level() {
        // gemini-3-pro lists only low and high.
        let m = ladder(&[Effort::Low, Effort::High]);
        assert_eq!(clamp_effort(Effort::Medium, &m), Effort::Low);
        assert_eq!(clamp_effort(Effort::Xhigh, &m), Effort::High);
        assert_eq!(clamp_effort(Effort::Minimal, &m), Effort::Low);
        // A model with no ladder leaves the request alone; resolve_reasoning
        // is what refuses it.
        assert_eq!(clamp_effort(Effort::High, &ladder(&[])), Effort::High);
    }

    #[test]
    fn effort_names_round_trip_and_serialize_lowercase() {
        for level in EFFORT_LADDER {
            assert_eq!(Effort::from_name(level.name()), Some(level));
            assert_eq!(
                serde_json::to_value(level).expect("serializes"),
                serde_json::json!(level.name())
            );
        }
        assert_eq!(Effort::from_name("XHIGH"), Some(Effort::Xhigh));
        assert_eq!(Effort::from_name("off"), None);
        assert_eq!(Effort::Xhigh.to_string(), "xhigh");
    }

    fn asked(effort: Effort) -> RequestOpts {
        RequestOpts {
            effort: Some(effort),
        }
    }

    #[test]
    fn nothing_is_resolved_without_a_request_or_a_declared_model() {
        let m = ModelCompat::default();
        for api in [
            ApiKind::OpenAiCompletions,
            ApiKind::OpenAiResponses,
            ApiKind::AnthropicMessages,
            ApiKind::GeminiGenerateContent,
        ] {
            assert_eq!(
                resolve_reasoning(api, Some(&m), &RequestOpts::default()),
                None,
                "{api}: no effort asked for"
            );
            assert_eq!(
                resolve_reasoning(api, None, &asked(Effort::High)),
                None,
                "{api}: a model that declares nothing takes nothing"
            );
            assert_eq!(
                resolve_reasoning(api, Some(&ladder(&[])), &asked(Effort::High)),
                None,
                "{api}: an empty ladder accepts no level"
            );
        }
    }

    #[test]
    fn a_dialect_of_another_family_or_without_a_builder_resolves_to_nothing() {
        let with = |format| ModelCompat {
            thinking_format: Some(format),
            ..ModelCompat::default()
        };
        assert_eq!(
            resolve_reasoning(
                ApiKind::OpenAiCompletions,
                Some(&with(ThinkingFormat::GoogleLevel)),
                &asked(Effort::High)
            ),
            None
        );
        for format in [
            ThinkingFormat::Zai,
            ThinkingFormat::Qwen,
            ThinkingFormat::OpenRouter,
        ] {
            assert_eq!(
                resolve_reasoning(
                    ApiKind::OpenAiCompletions,
                    Some(&with(format)),
                    &asked(Effort::High)
                ),
                None
            );
        }
    }

    #[test]
    fn the_family_default_dialect_and_the_ladder_decide_what_is_sent() {
        let m = ladder(&[Effort::Low, Effort::Medium, Effort::High]);
        let got = resolve_reasoning(ApiKind::AnthropicMessages, Some(&m), &asked(Effort::Xhigh))
            .expect("declared");
        assert_eq!(got.format, ThinkingFormat::AnthropicAdaptive);
        assert_eq!(got.effort, Effort::High);
        assert_eq!(got.value, ReasoningValue::Token("high".into()));
    }

    #[test]
    fn an_effort_map_entry_overrides_the_wire_token() {
        let m = ModelCompat {
            effort_map: vec![(Effort::Low, "minimal".into())],
            ..ModelCompat::default()
        };
        let got = resolve_reasoning(ApiKind::OpenAiResponses, Some(&m), &asked(Effort::Low))
            .expect("declared");
        assert_eq!(got.value, ReasoningValue::Token("minimal".into()));
    }

    #[test]
    fn a_budget_dialect_resolves_to_thought_tokens() {
        let m = ModelCompat {
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            thinking_format: Some(ThinkingFormat::GoogleBudget),
            ..ModelCompat::default()
        };
        let got = resolve_reasoning(
            ApiKind::GeminiGenerateContent,
            Some(&m),
            &asked(Effort::Medium),
        )
        .expect("declared");
        assert_eq!(got.value, ReasoningValue::Budget(8_192));
    }
}
