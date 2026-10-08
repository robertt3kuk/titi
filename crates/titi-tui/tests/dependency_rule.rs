//! Dependency rule: no module of `titi-tui` may depend on
//! `titi-providers` / `titi-tools`.
//!
//! Contract: `docs/research/tui-renderer/README.md` — "ни один модуль
//! titi-tui не зависит от titi-providers/titi-tools".
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;

fn manifest() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(dir.join("Cargo.toml")).expect("read titi-tui Cargo.toml")
}

#[test]
fn no_dependency_on_titi_providers() {
    let manifest = manifest();
    assert!(
        !manifest.contains("titi-providers"),
        "titi-tui must not depend on titi-providers"
    );
}

#[test]
fn no_dependency_on_titi_tools() {
    let manifest = manifest();
    assert!(
        !manifest.contains("titi-tools"),
        "titi-tui must not depend on titi-tools"
    );
}
