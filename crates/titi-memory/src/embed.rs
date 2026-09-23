//! Where a memory's vector comes from.
//!
//! The local embedder is a hashed bag of character trigrams: no model, no
//! network, and the same text always maps to the same vector. A configured
//! model replaces it. The index stores which embedder produced each vector, so
//! a switch of model does not compare vectors from two different spaces.

use sha2::{Digest, Sha256};

use crate::index::EMBED_DIM;

/// Something that turns text into a vector.
///
/// `name` is stored beside the vector. Recall only compares vectors that share
/// a name, because cosine across two models is noise.
pub trait Embedder: Send + Sync {
    fn name(&self) -> &str;
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// The default. Hashed character trigrams, L2-normalised, fixed dimension.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalEmbedder;

impl Embedder for LocalEmbedder {
    fn name(&self) -> &str {
        "local-trigram-v1"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0f32; EMBED_DIM];
        let lower: String = text
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let chars: Vec<char> = lower.chars().collect();
        if chars.len() < 3 {
            return vec;
        }
        for window in chars.windows(3) {
            let tri: String = window.iter().collect();
            let hash = hash_trigram(&tri);
            let bucket = (hash as usize) % EMBED_DIM;
            let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
            vec[bucket] += sign;
        }
        normalise(&mut vec);
        vec
    }
}

/// A vector a remote model returned.
///
/// The index never calls the network itself. The caller embeds with whatever
/// provider the setting names and hands the vector in, so a failed request
/// degrades to the local embedder instead of failing the write.
#[derive(Debug, Clone)]
pub struct ProvidedVector {
    pub model: String,
    pub vector: Vec<f32>,
}

impl Embedder for ProvidedVector {
    fn name(&self) -> &str {
        &self.model
    }

    fn embed(&self, _text: &str) -> Vec<f32> {
        let mut vector = self.vector.clone();
        normalise(&mut vector);
        vector
    }
}

/// The setting that picks the embedder.
///
/// `memory.embeddingModel` in the agent config. Empty or `local` keeps the
/// trigram embedder. Anything else names an OpenAI-compatible embeddings
/// model, resolved through the same provider registry as chat.
pub fn configured_model(settings_value: Option<&str>) -> EmbeddingChoice {
    match settings_value.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("local") => EmbeddingChoice::Local,
        Some(model) => EmbeddingChoice::Model(model.to_owned()),
    }
}

/// A model the settings UI can offer.
///
/// `local` is always right: it needs nothing. The rest are the embeddings
/// models a provider actually ships, named the way the registry names them,
/// so a choice copies straight into `memory.embeddingModel`.
pub struct EmbeddingSuggestion {
    pub id: &'static str,
    pub provider: &'static str,
    pub note: &'static str,
}

/// What to suggest. Ordered: the offline default first, then the cheapest
/// hosted model per provider.
pub const SUGGESTED_EMBEDDERS: &[EmbeddingSuggestion] = &[
    EmbeddingSuggestion {
        id: "local",
        provider: "built-in",
        note: "offline, no key, trigram vectors",
    },
    EmbeddingSuggestion {
        id: "openai/text-embedding-3-small",
        provider: "openai",
        note: "cheap, 1536d",
    },
    EmbeddingSuggestion {
        id: "openai/text-embedding-3-large",
        provider: "openai",
        note: "best quality, 3072d",
    },
    EmbeddingSuggestion {
        id: "voyage/voyage-3-lite",
        provider: "voyage",
        note: "cheap, retrieval tuned",
    },
    EmbeddingSuggestion {
        id: "voyage/voyage-3",
        provider: "voyage",
        note: "best retrieval quality",
    },
    EmbeddingSuggestion {
        id: "cohere/embed-v4",
        provider: "cohere",
        note: "multilingual",
    },
];

/// The suggestions a user can actually use.
///
/// `local` is always offered. A hosted model is offered only when its provider
/// is connected, because suggesting a model with no key is a dead end. An
/// empty `connected` means nothing is configured, so only `local` shows.
pub fn available<'a>(connected: &[&str]) -> Vec<&'a EmbeddingSuggestion> {
    SUGGESTED_EMBEDDERS
        .iter()
        .filter(|s| s.provider == "built-in" || connected.contains(&s.provider))
        .collect()
}

/// The suggestion lines, one per usable model.
pub fn suggested_lines(connected: &[&str]) -> Vec<String> {
    available(connected)
        .iter()
        .map(|s| format!("{}  {} — {}", s.id, s.provider, s.note))
        .collect()
}

/// Which embedder the config asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingChoice {
    /// The built-in trigram embedder.
    Local,
    /// An OpenAI-compatible embeddings model id.
    Model(String),
}

fn normalise(vec: &mut [f32]) {
    let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
}

fn hash_trigram(tri: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(tri.as_bytes());
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes[..8].try_into().unwrap_or([0; 8]))
}

/// Cosine of two equal-length vectors. Zero when the lengths differ, which is
/// what keeps vectors from two models from scoring against each other.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_text_embeds_identically() {
        let embedder = LocalEmbedder;
        assert_eq!(embedder.embed("auth token"), embedder.embed("auth token"));
    }

    #[test]
    fn related_text_scores_above_unrelated_text() {
        let embedder = LocalEmbedder;
        let auth = embedder.embed("the auth token expires too early");
        let again = embedder.embed("auth token expiry is too early");
        let readme = embedder.embed("the readme explains the build");
        assert!(cosine(&auth, &again) > cosine(&auth, &readme));
    }

    #[test]
    fn an_empty_setting_stays_local() {
        assert_eq!(configured_model(None), EmbeddingChoice::Local);
        assert_eq!(configured_model(Some("")), EmbeddingChoice::Local);
        assert_eq!(configured_model(Some("local")), EmbeddingChoice::Local);
    }

    #[test]
    fn only_connected_providers_are_offered() {
        let none = suggested_lines(&[]);
        assert_eq!(
            none.len(),
            1,
            "nothing connected offers only local: {none:?}"
        );
        assert!(none[0].starts_with("local"));

        let openai = suggested_lines(&["openai"]);
        assert!(openai.iter().any(|l| l.contains("text-embedding-3-small")));
        assert!(
            openai.iter().all(|l| !l.contains("voyage")),
            "an unconnected provider is not offered: {openai:?}"
        );
    }

    #[test]
    fn a_named_model_is_kept_verbatim() {
        assert_eq!(
            configured_model(Some("openai/text-embedding-3-small")),
            EmbeddingChoice::Model("openai/text-embedding-3-small".into())
        );
    }

    #[test]
    fn vectors_of_different_lengths_do_not_score() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }
}
