//! Privacy settings: the user's config decides, a project can only tighten.
//!
//! `atlas.limit` shares the same load path: an out-of-range or non-integer
//! value keeps the engine default instead of failing startup.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

use titi_cli::engine::{atlas_budget_from, privacy_policy};
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

#[test]
fn atlas_budget_read_from_settings() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&agent.path().join("config.yml"), "atlas:\n  budget: 600\n");
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(atlas_budget_from(&settings), Some(600));
}

#[test]
fn a_project_can_set_its_own_atlas_budget() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&agent.path().join("config.yml"), "atlas:\n  budget: 600\n");
    write(
        &project.path().join(PROJECT_SUBPATH),
        "atlas:\n  budget: 900\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(atlas_budget_from(&settings), Some(900));
}

#[test]
fn atlas_budget_missing_is_sized_from_the_repository() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(atlas_budget_from(&settings), None);
}

/// The old key is gone, not aliased: a config that still says `atlas.limit`
/// gets the automatic budget.
#[test]
fn the_old_limit_key_is_not_read() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&agent.path().join("config.yml"), "atlas:\n  limit: 4\n");
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(atlas_budget_from(&settings), None);
}

#[test]
fn atlas_budget_out_of_range_or_not_integer_is_automatic() {
    for raw in [
        "atlas:\n  budget: 0\n",
        "atlas:\n  budget: 199\n",
        "atlas:\n  budget: 4001\n",
        "atlas:\n  budget: no\n",
    ] {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(&agent.path().join("config.yml"), raw);
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(atlas_budget_from(&settings), None, "for {raw:?}");
    }
}

#[test]
fn atlas_enabled_from_missing_key() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::atlas_enabled_from(&settings));
}

#[test]
fn atlas_enabled_from_bool_true() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "atlas:\n  enabled: true\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::atlas_enabled_from(&settings));
}

#[test]
fn atlas_enabled_from_bool_false() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "atlas:\n  enabled: false\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::atlas_enabled_from(&settings));
}

#[test]
fn atlas_enabled_from_string_off() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "atlas:\n  enabled: \"off\"\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::atlas_enabled_from(&settings));
}

#[test]
fn atlas_enabled_from_string_maybe() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "atlas:\n  enabled: \"maybe\"\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::atlas_enabled_from(&settings));
}

#[test]
fn atlas_enabled_project_overrides_global() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "atlas:\n  enabled: true\n",
    );
    write(
        &project.path().join(PROJECT_SUBPATH),
        "atlas:\n  enabled: false\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::atlas_enabled_from(&settings));
}
