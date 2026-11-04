#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results
cargo +1.97.1 --version | tee results/toolchain.log
rustc +1.97.1 --version | tee -a results/toolchain.log
cargo +1.97.1 test --manifest-path Cargo.toml --locked -- --show-output 2>&1 | tee results/tests-1.97.1.log
cargo +1.97.1 clippy --manifest-path Cargo.toml --locked --all-targets --no-deps -- -D warnings 2>&1 | tee results/clippy-1.97.1.log
