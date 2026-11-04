#!/usr/bin/env bash
# Build an unchanged control reader from exact locked Loro source, then test E/R.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
baseline=531a959f5322f9fd42f30b5de65f1047a721b498
toolchain=${LORO_TEST_TOOLCHAIN:-1.90.0}
artifacts=${LORO_REVIEW_ARTIFACTS:-${TMPDIR:-/tmp}/loro-update-reuse-independent}
mkdir -p "$artifacts"
artifacts=$(cd "$artifacts" && pwd)
control="$artifacts/control-source"
if ! git -C "$root" cat-file -e "$baseline^{commit}"; then
  echo "Fetch baseline commit $baseline from origin before running this check." >&2
  exit 1
fi
if [[ -e "$control" ]]; then
  echo "Choose a fresh LORO_REVIEW_ARTIFACTS directory: $control already exists." >&2
  exit 1
fi
mkdir "$control"
git -C "$root" archive "$baseline" | tar -x -C "$control"
mkdir -p "$control/crates/loro/examples"
cp "$root/crates/loro/tests/support/update_reuse_baseline_reader.rs" "$control/crates/loro/examples/independent_reader.rs"
printf 'baseline=%s\ncandidate=%s\ntoolchain=%s\n' "$baseline" "$(git -C "$root" rev-parse HEAD)" "$toolchain" > "$artifacts/revisions.txt"
CARGO_TARGET_DIR="$artifacts/control-target" cargo "+$toolchain" build --locked \
  --manifest-path "$control/Cargo.toml" -p loro --example independent_reader 2>&1 | tee "$artifacts/control-build.log"
LORO_BASELINE_READER="$artifacts/control-target/debug/examples/independent_reader" \
  cargo "+$toolchain" test --locked --manifest-path "$root/Cargo.toml" -p loro \
  --test update_reuse_independent -- --include-ignored --nocapture 2>&1 | tee "$artifacts/independent-tests.log"
