#!/usr/bin/env bash
# Scores ask() against evals/cases.json. Needs TYPESAFE_API_KEY in .env.
# Pass --verbose to see the query behind every task, not only the failures.
# Pass a file to score a different task set: ./eval.sh evals/holdout.json
set -euo pipefail
cd "$(dirname "$0")"
set -a; . ./.env; set +a
cargo run --quiet --example eval -- "$@"
