//! Privacy settings: the user's config decides, a project can only tighten.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use titi_cli::engine::privacy_policy;
use titi_config::settings::{PROJECT_SUBPATH, Settings};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn defaults_mask_addresses_and_use_the_built_in_list() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    let (policy, mask_ips) = privacy_policy(&settings);
    assert!(mask_ips);
    assert!(policy.blocks(Path::new(".env")));
}

#[test]
fn a_project_cannot_loosen_privacy() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join(PROJECT_SUBPATH),
        "privacy:\n  maskIps: false\n  allow: [\".env\"]\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    let (policy, mask_ips) = privacy_policy(&settings);
    assert!(mask_ips, "project turned masking off");
    assert!(policy.blocks(Path::new(".env")), "project opened .env");
}

#[test]
fn a_project_can_add_sensitive_files() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "privacy:\n  sensitive: [\"*.vault\"]\n",
    );
    write(
        &project.path().join(PROJECT_SUBPATH),
        "privacy:\n  sensitive: [\"deploy/prod.yml\"]\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    let (policy, _) = privacy_policy(&settings);
    assert!(policy.blocks(Path::new("team.vault")));
    assert!(policy.blocks(Path::new("deploy/prod.yml")));
}

#[test]
fn the_user_can_loosen_their_own_privacy() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "privacy:\n  maskIps: false\n  allow: [\".env.test\"]\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    let (policy, mask_ips) = privacy_policy(&settings);
    assert!(!mask_ips);
    assert!(!policy.blocks(Path::new(".env.test")));
    assert!(policy.blocks(Path::new(".env")));
}
