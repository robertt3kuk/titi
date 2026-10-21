#!/usr/bin/env bash
set -euo pipefail
if grep -rq --include='*.py' 'calc_total' .; then
  echo "calc_total still referenced" >&2
  exit 1
fi
python3 -m unittest -q
