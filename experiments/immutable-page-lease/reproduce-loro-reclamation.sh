#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results
log=results/loro-reclamation-1.97.1.log
{
  cargo +1.97.1 --version
  rustc +1.97.1 --version
  sha256sum Cargo.toml Cargo.lock src/loro_reclamation.rs src/rooted_reclamation.rs src/reclamation_probe.rs src/store.rs ../../crates/loro-internal/src/trusted_history.rs
  cargo +1.97.1 test --manifest-path Cargo.toml --locked -- --show-output
  cargo +1.97.1 check --manifest-path Cargo.toml --locked --lib
} 2>&1 | tee "$log"
grep '^MEASUREMENT ' "$log" | sed 's/^MEASUREMENT //' > results/loro-reclamation.jsonl
