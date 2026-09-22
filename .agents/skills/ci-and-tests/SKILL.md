---
name: ci-and-tests
description: How verification works in titi — tests, fmt, and clippy run on GitHub Actions, not locally. Use when you need to check whether a change passed, or read why a CI job failed.
---

# ci-and-tests

Verification for titi happens on GitHub Actions, never on a contributor's
machine — nothing is meant to compile locally. The workflow is
`.github/workflows/ci.yml` with three jobs on every push and on PRs into
master: `fmt` (`cargo fmt --all --check`), `clippy` (`cargo clippy
--workspace --all-targets`, not denied), and `test` (`cargo test
--workspace --locked`).

## Read the results with gh

```bash
gh run list --repo robertt3kuk/titi --commit "$(git rev-parse HEAD)"
gh run list --repo robertt3kuk/titi --branch master --limit 5
gh run watch <run-id> --repo robertt3kuk/titi --exit-status
gh run view <run-id> --repo robertt3kuk/titi
gh run view <run-id> --repo robertt3kuk/titi --log-failed
```

A `cancelled` run was superseded by a newer push (`cancel-in-progress`);
read the latest run instead. Exceptions to "nothing local": TUI checks
(skill `tui-smoke`) and the genome map (skill `genome-map`).

`gh run watch --exit-status` blocks until the run finishes and exits
non-zero if it failed. `--log-failed` prints only the failing steps —
start there when a job is red, fix the real cause, and push another
focused commit. Never weaken a test to make it pass.

## Commit discipline

See `docs/COMMITS.md`: conventional `type(scope): summary`, one concern
per commit, prose bodies, straight-to-master push, never force-push.
