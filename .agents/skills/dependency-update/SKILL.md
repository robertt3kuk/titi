---
name: dependency-update
description: Add, bump, or remove a crate dependency in the titi workspace safely — justification, RustSec check, lockfile, one commit. Use when a change needs a new crate or a version bump.
---

# dependency-update

## Before adding
1. Can std or an existing workspace crate do it? Check `Cargo.toml` files
   first (`serde`, `tokio`, `thiserror`, `regex`, `reqwest`, `rusqlite` …
   are already in use).
2. Check the crate: last release date, downloads, maintainers, license
   (must be MIT/Apache-compatible), `unsafe` surface.
3. RustSec: search https://rustsec.org/advisories/ for the crate name, or
   `cargo audit` if it is installed (don't install tools without asking).
4. Minimal features: `default-features = false` when defaults pull in
   unneeded stacks (see `reqwest` with `rustls-tls`).

## Adding
- Shared by 2+ crates → consider `[workspace.dependencies]` in the root
  `Cargo.toml`; the root manifest and `Cargo.lock` are shared files — one
  writer at a time (skill `supervisor-review`).
- Keep `Cargo.lock` committed; CI runs `cargo test --workspace --locked`, so
  a stale lockfile fails CI.

## Commit
One commit per dependency change: `chore(cargo): add <crate> for <why>` or
`chore(<crate>): bump <dep> to <ver>`. Body: why this crate, what was
checked (advisories, maintenance), alternative rejected. Then skill
`commit` and watch CI (skill `ci-and-tests`).

## Removing
Grep for the crate across `crates/`, remove it, let CI prove nothing broke.
