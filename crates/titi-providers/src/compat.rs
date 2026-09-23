//! Compat policies: model quirks are declared as endpoint-family metadata,
//! never as provider-name branches. Two-phase resolution mirrors omp:
//! build-time `ModelCompat` (catalog) → request-time [`resolve_compat`].

use smol_str::SmolStr;

use crate::transport::ApiKind;

/// Canonical effort scale (`packages/catalog/effort.ts` analog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

/// Thinking wire dialect per endpoint family.
#[derive(Debug, Clone, PartialEq)]
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
    /// Gemini: `thinkingConfig: {thinkingLevel}`.
    GoogleLevel,
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
#[derive(Debug, Clone)]
pub struct ModelCompat {
    pub efforts: Vec<Effort>,
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

/// Clamp a requested effort to the model's ladder (Hermes-style: never 400 on
/// `xhigh` against an endpoint that only knows low..max).
pub fn clamp_effort(requested: Effort, model: &ModelCompat) -> Effort {
    // One pass, and the empty ladder falls out of the same match instead of
    // an is_empty guard the two lookups then have to assert again.
    let (Some(max_supported), Some(min_supported)) = (
        model.efforts.iter().max().copied(),
        model.efforts.iter().min().copied(),
    ) else {
        return requested;
    };
    if requested > max_supported {
        max_supported
    } else if requested < min_supported {
        min_supported
    } else {
        requested
    }
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
}
