#!/usr/bin/env bash
# Run every parseh-fuzz target sequentially.
#
#   TIME_BUDGET=60 ./run-all.sh
#
# Defaults to 60 s per target (7 targets · 60 s ≈ 7 minutes wall clock).
# Bump TIME_BUDGET for nightly soak runs.
#
# cargo-fuzz requires nightly Rust. If your default toolchain is
# stable, `cargo +nightly fuzz` is the way through. The script does
# NOT install nightly; failure to find it is your signal to run
#
#   rustup toolchain install nightly
#
# Exits non-zero on the first crash. The crash artifact is recorded
# at `fuzz/artifacts/<target>/`. See the README for the analysis
# workflow.
set -euo pipefail

TIME_BUDGET="${TIME_BUDGET:-60}"
TARGETS=(
  fuzz_job_spec
  fuzz_job_result
  fuzz_job_verification
  fuzz_job_outcome
  fuzz_capability_advertisement
  fuzz_state_delta
  fuzz_signature_verify
)

cd "$(dirname "$0")"

for t in "${TARGETS[@]}"; do
  echo "→ ${t} for ${TIME_BUDGET}s"
  cargo +nightly fuzz run "${t}" -- -max_total_time="${TIME_BUDGET}" || exit 1
done

echo "✓ all targets clean (${#TARGETS[@]} targets · ${TIME_BUDGET}s each)"
