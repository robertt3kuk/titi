//! The working-tree diff rides the turn the provider sees, and a repo-blind
//! duck turn does not receive it.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use titi_engine::protocol::SessionMode;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{MockBody, MockTransport, Role, StopReason, StreamEvent, Transport};

struct MapResolver(HashMap<String, Arc<dyn Transport>>);

impl TransportResolver for MapResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.0
            .get(model)
            .cloned()
            .map(|transport| ResolvedModel::without_credential(model, transport))
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))
    }
}

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(HashMap::from([(
        "primary".to_owned(),
        transport,
    )])))
}

async fn collect_until_terminal(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let terminal = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=titi@example.invalid"])
        .args(["-c", "user.name=titi test"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Hub outranks leaf until something touches leaf. The diff is that touch:
/// the user edited the file outside the agent, so the map must lead with it
/// and the prompt must show the added line.
fn repo_with_an_outside_edit() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "master"]);
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/hub.rs"), "pub fn hub() {}\n").expect("write");
    fs::write(
        root.join("src/leaf.rs"),
        "pub use crate::hub::hub;\npub fn leaf() {}\n",
    )
    .expect("write");
    git(root, &["add", "src"]);
    git(root, &["commit", "-qm", "first"]);
    fs::write(
        root.join("src/leaf.rs"),
        "pub use crate::hub::hub;\npub fn leaf() {}\npub fn added_for_the_turn() {}\n",
    )
    .expect("write");
    fs::write(root.join(".env"), "API_KEY=sk-test\n").expect("write");
    git(root, &["add", "-f", ".env"]);
    dir
}

#[tokio::test]
async fn the_turn_shows_the_working_tree_diff_and_boosts_the_edited_file() {
    let workspace = repo_with_an_outside_edit();
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.genome_root = Some(workspace.path().to_path_buf());
    let mut engine = EngineRuntime::start(config, resolver(transport));
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "what changed".into(),
        })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let requests = captured.requests();
    let prompt = requests[0].messages.last().expect("a prompt");
    assert_eq!(prompt.role, Role::User);
    assert!(prompt.content.contains("<diff>"), "{}", prompt.content);
    assert!(
        prompt.content.contains("+pub fn added_for_the_turn() {}"),
        "{}",
        prompt.content
    );
    assert!(prompt.content.contains("</diff>"), "{}", prompt.content);
    let genome_at = prompt.content.find("</genome>").expect("genome");
    let diff_at = prompt.content.find("<diff>").expect("diff");
    assert!(genome_at < diff_at, "{}", prompt.content);
    assert!(!prompt.content.contains("sk-test"), "{}", prompt.content);
    assert!(!prompt.content.contains(".env"), "{}", prompt.content);

    let map = &prompt.content[..genome_at];
    let leaf_at = map.find("src/leaf.rs").expect("leaf in the map");
    let hub_at = map.find("src/hub.rs").expect("hub in the map");
    assert!(
        leaf_at < hub_at,
        "a file edited outside the agent must lead the map:\n{}",
        prompt.content
    );
}

#[tokio::test]
async fn a_duck_turn_is_not_shown_the_working_tree() {
    let workspace = repo_with_an_outside_edit();
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.mode = SessionMode::Duck;
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.genome_root = Some(workspace.path().to_path_buf());
    let mut engine = EngineRuntime::start(config, resolver(transport));
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "talk me through it".into(),
        })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let requests = captured.requests();
    let prompt = requests[0].messages.last().expect("a prompt");
    assert!(
        !prompt.content.contains("<diff>"),
        "a duck turn was handed the working tree: {}",
        prompt.content
    );
    assert!(
        !prompt.content.contains("added_for_the_turn"),
        "{}",
        prompt.content
    );
    assert!(!prompt.content.contains("sk-test"), "{}", prompt.content);
}
