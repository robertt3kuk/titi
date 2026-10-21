#!/usr/bin/env bash
# Run in the task's working copy. Exit 0 = solved.
set -euo pipefail
cargo test --offline --quiet
