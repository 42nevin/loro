#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results
log=results/reclamation-lifecycle-1.97.1.log
{
  cargo +1.97.1 --version
  rustc +1.97.1 --version
  sha256sum src/reclamation_probe.rs src/store.rs
  cargo +1.97.1 test --manifest-path Cargo.toml --locked -- --show-output
} 2>&1 | tee "$log"
grep '^MEASUREMENT ' "$log" | sed 's/^MEASUREMENT //' > results/reclamation-lifecycle.jsonl
