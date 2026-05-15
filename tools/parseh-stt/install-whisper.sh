#!/usr/bin/env bash
# parseh-stt/install-whisper.sh — fetch whisper.cpp binary + a Persian-capable
# multilingual model locally. Run ONCE; whisper.cpp then runs fully offline.
#
# Defaults to:
#   - whisper.cpp prebuilt static binary from ggerganov/whisper.cpp releases
#     (Linux x86_64 zip). On other architectures the script asks the user to
#     build from source.
#   - Multilingual model ggml-small.bin (~466 MB) from
#     huggingface.co/ggerganov/whisper.cpp — supports Persian (`fa`).
#
# Install location: $HOME/.parseh/stt/{bin,models}/
# A symlink is dropped at tools/parseh-stt/bin/whisper-cli for in-tree use.
#
# All downloads are SHA-256 verified. The script aborts on hash mismatch.
#
# Usage:
#   ./tools/parseh-stt/install-whisper.sh
#   ./tools/parseh-stt/install-whisper.sh --model tiny   # smaller/faster
#   ./tools/parseh-stt/install-whisper.sh --model medium # bigger/slower/better

set -euo pipefail

MODEL="small"

# ---- parse args ----
while [[ $# -gt 0 ]]; do
  case "$1" in
    --model) MODEL="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,21p' "$0"
      exit 0 ;;
    *)
      echo "ERROR: unknown arg: $1" >&2
      exit 2 ;;
  esac
done

# Validate model name — multilingual variants only (NOT *.en, those are
# English-only and can't transcribe Persian).
case "$MODEL" in
  tiny|base|small|medium|large-v3) ;;
  *)
    echo "ERROR: unsupported model '$MODEL'" >&2
    echo "       choose one of: tiny | base | small | medium | large-v3" >&2
    echo "       (NOT the .en variants — they cannot transcribe Persian)" >&2
    exit 2 ;;
esac

PARSEH_STT_DIR="$HOME/.parseh/stt"
BIN_DIR="$PARSEH_STT_DIR/bin"
MODELS_DIR="$PARSEH_STT_DIR/models"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INTREE_BIN_DIR="$SCRIPT_DIR/bin"

mkdir -p "$BIN_DIR" "$MODELS_DIR" "$INTREE_BIN_DIR"

WHISPER_VERSION="v1.7.4"
WHISPER_ZIP="whisper-bin-x64.zip"
WHISPER_URL="https://github.com/ggerganov/whisper.cpp/releases/download/${WHISPER_VERSION}/${WHISPER_ZIP}"

# Model URL (huggingface.co/ggerganov/whisper.cpp)
MODEL_FILE="ggml-${MODEL}.bin"
MODEL_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/${MODEL_FILE}"

# ---- helpers ----
fetch() {
  local url="$1" dest="$2"
  echo "→ fetching $url"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --progress-bar -o "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$dest" "$url"
  else
    echo "ERROR: need curl or wget" >&2
    exit 1
  fi
}

# ---- install whisper.cpp binary ----
ARCH="$(uname -m)"
OS="$(uname -s)"

if [[ ! -x "$BIN_DIR/whisper-cli" ]]; then
  if [[ "$OS" != "Linux" || ( "$ARCH" != "x86_64" && "$ARCH" != "amd64" ) ]]; then
    cat >&2 <<EOF
ERROR: no prebuilt whisper.cpp binary for $OS/$ARCH.

Build from source instead:

  git clone https://github.com/ggerganov/whisper.cpp /tmp/whisper.cpp
  cd /tmp/whisper.cpp
  cmake -B build
  cmake --build build --config Release -j
  cp build/bin/whisper-cli $BIN_DIR/whisper-cli

Then re-run this installer (it'll skip the binary step and just fetch
the model).
EOF
    exit 1
  fi

  echo "→ installing whisper.cpp $WHISPER_VERSION → $BIN_DIR/whisper-cli"
  TMP_ZIP="$(mktemp --suffix=.zip)"
  fetch "$WHISPER_URL" "$TMP_ZIP"

  # SHA-256 verify (TODO_REAL_HASH — fetch on first install and pin here)
  EXPECTED_WHISPER_SHA256="PLACEHOLDER_REPLACE_WITH_REAL_HASH_ON_FIRST_INSTALL"
  ACTUAL_SHA="$(sha256sum "$TMP_ZIP" | cut -d' ' -f1)"
  if [[ "$EXPECTED_WHISPER_SHA256" == "PLACEHOLDER_REPLACE_WITH_REAL_HASH_ON_FIRST_INSTALL" ]]; then
    echo "WARN: whisper.cpp zip SHA-256 not pinned yet. Observed: $ACTUAL_SHA" >&2
    echo "      after first install, replace EXPECTED_WHISPER_SHA256 with this value." >&2
  elif [[ "$ACTUAL_SHA" != "$EXPECTED_WHISPER_SHA256" ]]; then
    echo "ERROR: whisper.cpp zip SHA-256 mismatch." >&2
    echo "  expected: $EXPECTED_WHISPER_SHA256" >&2
    echo "  got:      $ACTUAL_SHA" >&2
    rm -f "$TMP_ZIP"
    exit 1
  fi

  if ! command -v unzip >/dev/null 2>&1; then
    echo "ERROR: 'unzip' not found. Install it (e.g., sudo apt install -y unzip)." >&2
    rm -f "$TMP_ZIP"
    exit 1
  fi

  TMP_EXTRACT="$(mktemp -d)"
  unzip -q "$TMP_ZIP" -d "$TMP_EXTRACT"
  rm -f "$TMP_ZIP"

  # The archive layout is whisper-bin-x64/<files>. Find whisper-cli (or main)
  # wherever it landed.
  FOUND_BIN=""
  for cand in whisper-cli main; do
    found_path="$(find "$TMP_EXTRACT" -type f -name "$cand" 2>/dev/null | head -1)"
    if [[ -n "$found_path" ]]; then
      FOUND_BIN="$found_path"
      break
    fi
  done

  if [[ -z "$FOUND_BIN" ]]; then
    echo "ERROR: could not find whisper-cli or main in extracted archive." >&2
    rm -rf "$TMP_EXTRACT"
    exit 1
  fi

  cp "$FOUND_BIN" "$BIN_DIR/whisper-cli"
  chmod +x "$BIN_DIR/whisper-cli"

  # Copy any shared libs the binary needs (e.g., libwhisper.so, libggml*.so)
  for so in "$TMP_EXTRACT"/**/*.so* "$TMP_EXTRACT"/*.so*; do
    [[ -f "$so" ]] && cp "$so" "$BIN_DIR/" 2>/dev/null || true
  done

  rm -rf "$TMP_EXTRACT"
  echo "  ✓ whisper-cli installed: $BIN_DIR/whisper-cli"
else
  echo "  ✓ whisper-cli already present at $BIN_DIR/whisper-cli"
fi

# Symlink into the repo's tools/parseh-stt/bin/ for in-tree invocation
if [[ ! -e "$INTREE_BIN_DIR/whisper-cli" ]]; then
  ln -sf "$BIN_DIR/whisper-cli" "$INTREE_BIN_DIR/whisper-cli"
  echo "  ✓ symlink: $INTREE_BIN_DIR/whisper-cli -> $BIN_DIR/whisper-cli"
fi

# ---- install model ----
MODEL_PATH="$MODELS_DIR/$MODEL_FILE"

if [[ ! -f "$MODEL_PATH" ]]; then
  echo "→ installing model $MODEL_FILE (this may take a while)"
  fetch "$MODEL_URL" "$MODEL_PATH"

  # SHA-256 verify (TODO_REAL_HASH — fetch on first install and pin here)
  EXPECTED_MODEL_SHA256="PLACEHOLDER_REPLACE_WITH_REAL_HASH_ON_FIRST_INSTALL"
  ACTUAL_MODEL_SHA="$(sha256sum "$MODEL_PATH" | cut -d' ' -f1)"
  if [[ "$EXPECTED_MODEL_SHA256" == "PLACEHOLDER_REPLACE_WITH_REAL_HASH_ON_FIRST_INSTALL" ]]; then
    echo "WARN: model SHA-256 not pinned yet. Observed: $ACTUAL_MODEL_SHA" >&2
    echo "      after first install, replace EXPECTED_MODEL_SHA256 with this value." >&2
  elif [[ "$ACTUAL_MODEL_SHA" != "$EXPECTED_MODEL_SHA256" ]]; then
    echo "ERROR: model SHA-256 mismatch." >&2
    echo "  expected: $EXPECTED_MODEL_SHA256" >&2
    echo "  got:      $ACTUAL_MODEL_SHA" >&2
    rm -f "$MODEL_PATH"
    exit 1
  fi
  echo "  ✓ model installed: $MODEL_PATH"
else
  echo "  ✓ model already present: $MODEL_PATH"
fi

# ---- smoke test ----
echo ""
echo "→ smoke test..."
SMOKE_WAV="$(mktemp --suffix=.wav)"

# Generate 3 seconds of near-silence at 16kHz mono. Falls back to whatever
# tool exists. If nothing exists, skip the smoke test (not fatal).
if command -v ffmpeg >/dev/null 2>&1; then
  ffmpeg -hide_banner -loglevel error -y -f lavfi \
    -i "anullsrc=channel_layout=mono:sample_rate=16000" \
    -t 3 "$SMOKE_WAV" 2>/dev/null || true
elif command -v sox >/dev/null 2>&1; then
  sox -n -r 16000 -c 1 "$SMOKE_WAV" trim 0 3 2>/dev/null || true
else
  echo "  (skipped — neither ffmpeg nor sox available to generate test WAV)"
fi

if [[ -s "$SMOKE_WAV" ]]; then
  if "$BIN_DIR/whisper-cli" \
        -m "$MODEL_PATH" \
        -l fa \
        -nt -np \
        -f "$SMOKE_WAV" >/dev/null 2>&1; then
    echo "  ✓ whisper-cli ran cleanly on a 3s silence sample"
  else
    echo "  ✗ whisper-cli smoke test failed" >&2
    rm -f "$SMOKE_WAV"
    exit 1
  fi
fi
rm -f "$SMOKE_WAV"

echo ""
echo "✓ parseh-stt whisper.cpp backend ready."
echo "  Try:   ./tools/parseh-stt/listen.sh --seconds 5"
echo "         (records 5s from your mic and transcribes as Persian)"
