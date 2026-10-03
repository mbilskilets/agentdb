#!/usr/bin/env bash
# The single quality gate. Run before calling any change done.
set -euo pipefail
cd "$(dirname "$0")"

cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo deny check
