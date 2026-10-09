#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A price a user declares in `config.yml` is the price the engine charges.
//!
//! `ModelPrice` is micro-dollars per million tokens and lives on the model
//! descriptor, but nothing used to read one out of the user's own file: the
//! built-in table ships no prices on purpose and a configured model had no
//! field for one, so `/budget $` could never work over a model the user
//! declared. These are the two halves of that: a price that parses is on the
//! descriptor and costs what it says, and a price that does not is refused by
//! key without taking the model down with it.

use std::path::Path;
use titi_engine::ModelPrice;

/// An agent directory whose `config.yml` carries `price` for its one model.
fn user_config(root: &Path, price: &str) -> std::path::PathBuf {
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("config.yml"),
        format!(
            r#"
providers:
  - id: fake
    api: openai-completions
    base_url: http://127.0.0.1:18999/v1
    credential_required: false
models:
  - id: fake/scripted
    provider: fake
    wire_model: fake
    context_window: 32000
    price:
{price}
"#
        ),
    )
    .unwrap();
    agent
}

fn declared<'a>(
    config: &'a titi_engine::ProviderRegistryConfig,
) -> &'a titi_engine::ModelDescriptor {
    config
        .models
        .iter()
        .find(|model| model.id.as_str() == "fake/scripted")
        .expect("the declared model")
}

/// The whole chain: the file's dollars per million tokens become the
/// descriptor's micro-dollars, and a turn on it costs exactly what those
/// rates say.
#[test]
fn a_configured_price_is_what_a_turn_costs() {
    let root = tempfile::tempdir().unwrap();
    let agent = user_config(
        root.path(),
        "      input: 3\n      output: 15\n      cachedInput: 0.3",
    );
    let (config, problems) =
        titi_cli::engine::registry_config_for_with_problems(&agent, root.path());
    assert_eq!(problems, Vec::<String>::new());
    let price = declared(&config).price.expect("the declared price");
    assert_eq!(
        price,
        ModelPrice {
            input: 3_000_000,
            output: 15_000_000,
            cached_input: Some(300_000),
        }
    );
    // 100 prompt tokens, 60 of them cached, and 10 out: 40 at $3/MTok, 60 at
    // $0.30/MTok and 10 at $15/MTok, in micro-dollars.
    assert_eq!(price.cost_micro_usd(100, 60, 10), 40 * 3 + 60 * 300_000 / 1_000_000 + 10 * 15);
}

/// A price nobody can read is refused, and named: the model stays — unpriced,
/// which is not the same as free — and the rest of the config with it.
#[test]
fn an_unreadable_price_is_refused_by_key_and_leaves_the_model_unpriced() {
    for (price, expected) in [
        ("      input: -2\n      output: 15", "`input`"),
        ("      input: free\n      output: 15", "`input`"),
        ("      input: 0.0000001\n      output: 15", "`input`"),
        ("      input: 3", "`output` is missing"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let agent = user_config(root.path(), price);
        let (config, problems) =
            titi_cli::engine::registry_config_for_with_problems(&agent, root.path());
        let named = problems.join(" ");
        assert!(
            named.contains("models[0].price") && named.contains(expected),
            "{price} is not refused by name: {named:?}"
        );
        assert_eq!(
            declared(&config).price,
            None,
            "{price} must not become a number the ledger charges"
        );
        // The provider and model the user declared are still there: one bad
        // rate does not cost them the config.
        assert!(
            config
                .providers
                .iter()
                .any(|provider| provider.id.as_str() == "fake"),
            "the declared provider stands: {problems:?}"
        );
    }
}
