#!/usr/bin/env bash
# Build the Windows portable release for PARSEH V0.1-alpha.
#
# Usage:
#   ./tools/release/windows/build.sh [--with-inference] [--with-cli] [--target <triple>]
#
# Default target is x86_64-pc-windows-gnu (cross-compilable from Linux via
# mingw-w64). For an MSVC build, pass --target x86_64-pc-windows-msvc and
# run on a Windows host with the MSVC toolchain installed.
#
# Output:
#   dist/parseh-windows-v<VERSION>/                  staged portable tree
#   dist/parseh-windows-v<VERSION>.zip               distributable archive
#
# Honour env vars:
#   VERSION         override package version  (default: 0.1.0-alpha)
#   TARGET          override Rust target      (default: x86_64-pc-windows-gnu)
#   WITH_INFERENCE  set to 1 to also bundle parseh-inference.exe
#   WITH_CLI        set to 1 to also bundle parseh.exe (developer CLI)

set -euo pipefail

# ---------------------------------------------------------------------------
# RELEASE GUARD — SQLCipher encryption-at-rest (binding per
# the build policy "RELEASE-BLOCKING REQUIREMENT" 2026-05-14)
#
# parseh-shared-state ships with two Cargo features: "bundled" (plain SQLite,
# NOT for release) and "encrypted" (SQLCipher, REQUIRED for release). Any
# release binary that embeds parseh-shared-state MUST pin
# features = ["encrypted"] in its dependency declaration.
#
# This guard catches the obvious miss: if the miner depends on
# parseh-shared-state, that dependency line MUST include the "encrypted"
# feature. Defeated by trickier configurations; not a substitute for
# code review.
# ---------------------------------------------------------------------------
MINER_CARGO_TOML="$(cd "$(dirname "$0")" && cd ../../.. && pwd)/server/miner/Cargo.toml"
if grep -q 'parseh-shared-state' "$MINER_CARGO_TOML" 2>/dev/null; then
  # Look for the dependency declaration line and the next ~3 lines for the features array
  if ! awk '/parseh-shared-state/{flag=1; n=0} flag && n<5{print; n++}' "$MINER_CARGO_TOML" | grep -q 'features.*encrypted'; then
    echo "" >&2
    echo "✗ RELEASE GUARD FAILED: parseh-shared-state is depended on by parseh-miner" >&2
    echo "  but the 'encrypted' feature is not pinned in server/miner/Cargo.toml." >&2
    echo "" >&2
    echo "  This is a release-blocking defect" >&2
    echo "  (RELEASE-BLOCKING REQUIREMENT, 2026-05-14)." >&2
    echo "" >&2
    echo "  Fix: in server/miner/Cargo.toml, change the dependency line to:" >&2
    echo "    parseh-shared-state = { path = \"../parseh-shared-state\", features = [\"encrypted\"] }" >&2
    echo "" >&2
    exit 1
  fi
fi
# ---------------------------------------------------------------------------

VERSION="${VERSION:-0.1.0-alpha}"
TARGET="${TARGET:-x86_64-pc-windows-gnu}"
WITH_INFERENCE="${WITH_INFERENCE:-0}"
WITH_CLI="${WITH_CLI:-0}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
DIST_NAME="parseh-windows-v$VERSION"
DIST_DIR="$REPO_ROOT/dist/$DIST_NAME"
ZIP_PATH="$REPO_ROOT/dist/$DIST_NAME.zip"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-inference) WITH_INFERENCE=1; shift ;;
    --with-cli)       WITH_CLI=1; shift ;;
    --target)         TARGET="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,18p' "$0"
      exit 0
      ;;
    *) echo "Unknown flag: $1" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;34m→\033[0m %s\n' "$*"; }
err() { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; }

# ----- toolchain checks ---------------------------------------------------

if ! command -v rustup >/dev/null 2>&1; then
  err "rustup not found. Install Rust from https://rustup.rs/ and retry."
  exit 1
fi

if ! rustup target list --installed | grep -q "^${TARGET}$"; then
  log "Installing Rust target: $TARGET"
  rustup target add "$TARGET"
fi

if [[ "$TARGET" == *"-gnu" ]]; then
  if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    err "mingw-w64 not installed. On Debian/Ubuntu: apt install gcc-mingw-w64. On macOS: brew install mingw-w64."
    exit 1
  fi
  # Make rustc use the mingw linker for this target.
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-x86_64-w64-mingw32-gcc}"
fi

# ----- build --------------------------------------------------------------

build_crate() {
  local crate="$1"
  log "Building $crate for $TARGET..."
  if ! ( cd "$REPO_ROOT/server" && cargo build --release --target "$TARGET" -p "$crate" ); then
    err "cargo build failed for $crate on $TARGET."
    err "If the crate has a [lib] that isn't Windows-compatible yet (V0.1-alpha"
    err "still has rough edges), file an issue or build with --target"
    err "x86_64-pc-windows-msvc on a Windows host."
    exit 1
  fi
}

build_crate parseh-miner
if [[ "$WITH_INFERENCE" -eq 1 ]]; then
  build_crate parseh-inference
fi
if [[ "$WITH_CLI" -eq 1 ]]; then
  # parseh-cli's [[bin]] is named `parseh` (users type `parseh` in their
  # terminal); the crate name itself is parseh-cli for `cargo build -p`.
  build_crate parseh-cli
fi

# ----- stage the dist tree ------------------------------------------------

log "Assembling distribution at $DIST_DIR"
rm -rf "$DIST_DIR" "$ZIP_PATH"
mkdir -p "$DIST_DIR/examples"

MINER_EXE="$REPO_ROOT/server/target/$TARGET/release/parseh-miner.exe"
if [[ ! -f "$MINER_EXE" ]]; then
  err "Built artifact missing: $MINER_EXE"
  exit 1
fi
cp "$MINER_EXE" "$DIST_DIR/parseh-miner.exe"

if [[ "$WITH_INFERENCE" -eq 1 ]]; then
  INFER_EXE="$REPO_ROOT/server/target/$TARGET/release/parseh-inference.exe"
  if [[ ! -f "$INFER_EXE" ]]; then
    err "Built artifact missing: $INFER_EXE"
    exit 1
  fi
  cp "$INFER_EXE" "$DIST_DIR/parseh-inference.exe"
fi

if [[ "$WITH_CLI" -eq 1 ]]; then
  # The crate is parseh-cli; the bin output is `parseh.exe` (see
  # server/parseh-cli/Cargo.toml `[[bin]] name = "parseh"`).
  CLI_EXE="$REPO_ROOT/server/target/$TARGET/release/parseh.exe"
  if [[ ! -f "$CLI_EXE" ]]; then
    err "Built artifact missing: $CLI_EXE"
    exit 1
  fi
  cp "$CLI_EXE" "$DIST_DIR/parseh.exe"
fi

cp "$SCRIPT_DIR/README.txt"          "$DIST_DIR/README.txt"
cp "$REPO_ROOT/LICENSE"              "$DIST_DIR/LICENSE.txt"
cp "$SCRIPT_DIR/install-as-startup.bat" "$DIST_DIR/install-as-startup.bat"
cp "$SCRIPT_DIR/uninstall.bat"       "$DIST_DIR/uninstall.bat"
cp "$SCRIPT_DIR/miner.example.toml"  "$DIST_DIR/examples/miner.toml"

# Preserve sensible Unix bits; ZIP retains the mode for tools that read it.
chmod 755 "$DIST_DIR"/*.exe 2>/dev/null || true
chmod 644 "$DIST_DIR"/*.txt "$DIST_DIR"/*.bat "$DIST_DIR/examples/miner.toml"

# ----- sanity check binary sizes -----------------------------------------

for exe in "$DIST_DIR"/*.exe; do
  [[ -f "$exe" ]] || continue
  size=$(stat -c%s "$exe" 2>/dev/null || stat -f%z "$exe" 2>/dev/null || echo 0)
  printf '  %s: %d MB\n' "$(basename "$exe")" "$((size / 1024 / 1024))"
  if (( size < 1048576 )); then
    err "$exe is suspiciously small (<1MB). Did the build fail silently?"
    exit 1
  fi
done

# ----- zip ----------------------------------------------------------------

if ! command -v zip >/dev/null 2>&1; then
  err "zip not installed. apt install zip / brew install zip."
  exit 1
fi

log "Creating ZIP at $ZIP_PATH"
( cd "$REPO_ROOT/dist" && zip -r "$DIST_NAME.zip" "$DIST_NAME" >/dev/null )

# ----- final report -------------------------------------------------------

if command -v sha256sum >/dev/null 2>&1; then
  SHA="$(sha256sum "$ZIP_PATH" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
  SHA="$(shasum -a 256 "$ZIP_PATH" | cut -d' ' -f1)"
else
  SHA="(no sha256sum/shasum on PATH)"
fi

printf '\n'
printf '\033[1;32m✓\033[0m Built: %s\n' "$ZIP_PATH"
printf '  Size:    %s\n' "$(du -h "$ZIP_PATH" | cut -f1)"
printf '  SHA-256: %s\n' "$SHA"
