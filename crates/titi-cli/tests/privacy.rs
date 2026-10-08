//! Privacy settings: the user's config decides, a project can only tighten.
//!
//! `genome.limit` shares the same load path: an out-of-range or non-integer
//! value keeps the engine default instead of failing startup.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

use titi_cli::engine::{genome_limit_from, privacy_policy};
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
fn genome_limit_read_from_settings() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&agent.path().join("config.yml"), "genome:\n  limit: 4\n");
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(genome_limit_from(&settings), 4);
}

#[test]
fn a_project_can_set_its_own_genome_limit() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&agent.path().join("config.yml"), "genome:\n  limit: 4\n");
    write(
        &project.path().join(PROJECT_SUBPATH),
        "genome:\n  limit: 8\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(genome_limit_from(&settings), 8);
}

#[test]
fn genome_limit_missing_keeps_default() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(genome_limit_from(&settings), 24);
}

#[test]
fn genome_limit_out_of_range_or_not_integer_keeps_default() {
    for raw in [
        "genome:\n  limit: 0\n",
        "genome:\n  limit: 65\n",
        "genome:\n  limit: no\n",
    ] {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(&agent.path().join("config.yml"), raw);
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(genome_limit_from(&settings), 24, "for {raw:?}");
    }
}

#[test]
fn genome_enabled_from_missing_key() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::genome_enabled_from(&settings));
}

#[test]
fn genome_enabled_from_bool_true() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "genome:\n  enabled: true\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::genome_enabled_from(&settings));
}

#[test]
fn genome_enabled_from_bool_false() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "genome:\n  enabled: false\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::genome_enabled_from(&settings));
}

#[test]
fn genome_enabled_from_string_off() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "genome:\n  enabled: \"off\"\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::genome_enabled_from(&settings));
}

#[test]
fn genome_enabled_from_string_maybe() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "genome:\n  enabled: \"maybe\"\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(titi_cli::engine::genome_enabled_from(&settings));
}

#[test]
fn genome_enabled_project_overrides_global() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &agent.path().join("config.yml"),
        "genome:\n  enabled: true\n",
    );
    write(
        &project.path().join(PROJECT_SUBPATH),
        "genome:\n  enabled: false\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert!(!titi_cli::engine::genome_enabled_from(&settings));
}
