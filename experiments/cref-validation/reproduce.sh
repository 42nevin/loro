#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p experiments/cref-validation/results
export CARGO_INCREMENTAL=0
cargo +1.97.1 test --locked -p loro-internal --lib --features shared_arena_experiment 2>&1 | tee experiments/cref-validation/results/internal.log
cargo +1.97.1 test --locked -p loro --features shared_arena_experiment --test extent_restore --test shared_arena_experiment --test native_cow_fork 2>&1 | tee experiments/cref-validation/results/integration.log
cargo +1.97.1 check --locked -p loro-internal --all-features 2>&1 | tee experiments/cref-validation/results/all-features.log
