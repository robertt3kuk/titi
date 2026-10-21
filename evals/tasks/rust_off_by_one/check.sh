#!/usr/bin/env bash
# Run in the task working copy after hidden/ is copied over it. Exit 0 = solved.
set -euo pipefail
cargo test --offline --quiet --test sum
