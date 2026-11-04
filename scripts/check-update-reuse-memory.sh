#!/usr/bin/env bash
# Separate fixture generation from every measured fresh process. No latency claims.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${LORO_MEMORY_ARTIFACTS:?Set LORO_MEMORY_ARTIFACTS to a new output directory}"
mkdir -p "$LORO_MEMORY_ARTIFACTS"
artifacts=$(cd "$LORO_MEMORY_ARTIFACTS" && pwd)
if [[ -e "$artifacts/fixture/update.loro" || -e "$artifacts/observations.jsonl" ]]; then
  echo 'Choose a fresh output directory; refusing to mix runs.' >&2; exit 1
fi
cd "$root"
cargo "+${LORO_TEST_TOOLCHAIN:-1.90.0}" build --release --locked --manifest-path "$root/Cargo.toml" -p loro --example update_reuse_memory --message-format=json-render-diagnostics > "$artifacts/build.jsonl"
# Select this build's executable, including an explicitly configured target triple.
binary=$(python3 - "$artifacts/build.jsonl" <<'PYTHON'
import json, sys
paths = set()
for line in open(sys.argv[1]):
    row = json.loads(line)
    if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'update_reuse_memory' and row.get('executable'):
        paths.add(row['executable'])
assert len(paths) == 1, f'Expected one freshly resolved example executable, got {paths}'
print(paths.pop())
PYTHON
)
"$binary" prep "$artifacts/fixture" > "$artifacts/prep.log"
for repeat in 1 2 3 4 5; do
  for mode in full full_compact r_cold r_warm; do
    "$binary" "$mode" "$artifacts/fixture" >> "$artifacts/observations.jsonl"
  done
done
printf '%s\n' '20 fresh-process observations complete. Fixture construction ran only in prep.'
