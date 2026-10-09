#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The catalog's order decides which model a session runs on, and therefore
//! which one a subagent asks for.

use std::path::Path;

fn user_config(root: &Path) -> std::path::PathBuf {
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("config.yml"),
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
"#,
    )
    .unwrap();
    agent
}

/// A config that declares one model runs on it: the user's models lead the
/// catalog, and a builtin in front of them would be the session's model — and
/// the one a subagent is handed.
#[test]
fn a_declared_model_leads_the_catalog() {
    let root = tempfile::tempdir().unwrap();
    let agent = user_config(root.path());
    let config = titi_cli::engine::registry_config_for(&agent, root.path());
    let first = config.models.first().expect("a model");
    assert_eq!(first.id.as_str(), "fake/scripted");
    assert_eq!(first.provider.as_str(), "fake");
    assert_eq!(
        config
            .models
            .iter()
            .filter(|model| model.id.as_str() == "fake/scripted")
            .count(),
        1,
        "the declared model is not duplicated by the merge"
    );
    assert!(
        config.models.len() > 1,
        "the builtins stay in the catalog behind it: {:?}",
        config.models.iter().map(|m| m.id.to_string()).collect::<Vec<_>>()
    );
}
