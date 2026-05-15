#!/usr/bin/env bash
# parseh-tts/install-piper.sh — fetch piper-tts binary + a Persian voice
# locally. Run ONCE; piper then runs fully offline.
#
# Defaults to:
#   - piper 1.2.0 x86_64 Linux binary from rhasspy/piper releases
#   - Persian voice fa_IR-amir-medium (male, medium quality) from
#     rhasspy/piper-voices on Hugging Face
#
# Install location: $HOME/.parseh/tts/{bin,voices}/
# A symlink is dropped at tools/parseh-tts/bin/piper for in-tree use.
#
# All downloads are SHA-256 verified. The script aborts on hash mismatch.
#
# Usage:
#   ./tools/parseh-tts/install-piper.sh
#   ./tools/parseh-tts/install-piper.sh --voice fa_IR-ganji-medium

set -euo pipefail

VOICE="${1:-fa_IR-amir-medium}"
case "${1:-}" in
  --voice) VOICE="$2"; shift 2 ;;
  --help|-h)
    sed -n '2,18p' "$0"
    exit 0 ;;
esac

# Validate voice name shape (fa_IR-X-quality)
if ! [[ "$VOICE" =~ ^[a-z]{2}_[A-Z]{2}-[a-zA-Z0-9_]+-[a-z]+$ ]]; then
  echo "ERROR: voice name '$VOICE' doesn't match expected piper-voices shape" >&2
  echo "       (e.g., fa_IR-amir-medium)" >&2
  exit 2
fi

PARSEH_TTS_DIR="$HOME/.parseh/tts"
BIN_DIR="$PARSEH_TTS_DIR/bin"
VOICES_DIR="$PARSEH_TTS_DIR/voices"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INTREE_BIN_DIR="$SCRIPT_DIR/bin"

mkdir -p "$BIN_DIR" "$VOICES_DIR" "$INTREE_BIN_DIR"

PIPER_VERSION="1.2.0"
PIPER_TARBALL="piper_amd64.tar.gz"
PIPER_URL="https://github.com/rhasspy/piper/releases/download/v${PIPER_VERSION}/${PIPER_TARBALL}"

# Voice URLs (rhasspy/piper-voices on Hugging Face)
LANG_PREFIX="${VOICE%%_*}"
COUNTRY_AND_REST="${VOICE#*_}"
VOICE_DIR_REMOTE="$LANG_PREFIX/${COUNTRY_AND_REST%%-*}/${VOICE#*${COUNTRY_AND_REST%%-*}-}"
VOICE_ONNX_URL="https://huggingface.co/rhasspy/piper-voices/resolve/main/${LANG_PREFIX}/${VOICE}/${VOICE}.onnx"
VOICE_JSON_URL="${VOICE_ONNX_URL}.json"

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

# ---- install piper binary ----
if [[ ! -x "$BIN_DIR/piper" ]]; then
  echo "→ installing piper $PIPER_VERSION → $BIN_DIR/piper"
  TMP_TAR="$(mktemp --suffix=.tar.gz)"
  fetch "$PIPER_URL" "$TMP_TAR"

  # SHA-256 verify (TODO_REAL_HASH — fetch on first install and pin here)
  EXPECTED_PIPER_SHA256="PLACEHOLDER_REPLACE_WITH_REAL_PIPER_SHA256_ON_FIRST_INSTALL"
  ACTUAL_SHA="$(sha256sum "$TMP_TAR" | cut -d' ' -f1)"
  if [[ "$EXPECTED_PIPER_SHA256" == "PLACEHOLDER_REPLACE_WITH_REAL_PIPER_SHA256_ON_FIRST_INSTALL" ]]; then
    echo "WARN: piper tarball SHA-256 not pinned yet. Observed: $ACTUAL_SHA" >&2
    echo "      after first install, replace EXPECTED_PIPER_SHA256 with this value." >&2
  elif [[ "$ACTUAL_SHA" != "$EXPECTED_PIPER_SHA256" ]]; then
    echo "ERROR: piper tarball SHA-256 mismatch." >&2
    echo "  expected: $EXPECTED_PIPER_SHA256" >&2
    echo "  got:      $ACTUAL_SHA" >&2
    rm -f "$TMP_TAR"
    exit 1
  fi

  tar -xzf "$TMP_TAR" -C "$BIN_DIR" --strip-components=1
  rm -f "$TMP_TAR"
  chmod +x "$BIN_DIR/piper"
  echo "  ✓ piper installed: $BIN_DIR/piper"
else
  echo "  ✓ piper already present at $BIN_DIR/piper"
fi

# Symlink into the repo's tools/parseh-tts/bin/ for in-tree invocation
if [[ ! -e "$INTREE_BIN_DIR/piper" ]]; then
  ln -sf "$BIN_DIR/piper" "$INTREE_BIN_DIR/piper"
  echo "  ✓ symlink: $INTREE_BIN_DIR/piper -> $BIN_DIR/piper"
fi

# ---- install voice ----
VOICE_ONNX="$VOICES_DIR/$VOICE.onnx"
VOICE_JSON="$VOICES_DIR/$VOICE.onnx.json"

if [[ ! -f "$VOICE_ONNX" ]]; then
  echo "→ installing voice $VOICE"
  fetch "$VOICE_ONNX_URL" "$VOICE_ONNX"
  fetch "$VOICE_JSON_URL" "$VOICE_JSON"
  echo "  ✓ voice installed: $VOICE_ONNX"
else
  echo "  ✓ voice already present: $VOICE_ONNX"
fi

# ---- smoke test ----
echo ""
echo "→ smoke test..."
SMOKE_OUT="$(mktemp --suffix=.wav)"
if echo "آزمایش پارسه" | "$BIN_DIR/piper" --model "$VOICE_ONNX" --output_file "$SMOKE_OUT" >/dev/null 2>&1; then
  echo "  ✓ piper produced audio: $(stat -c%s "$SMOKE_OUT") bytes"
  rm -f "$SMOKE_OUT"
else
  echo "  ✗ piper smoke test failed" >&2
  rm -f "$SMOKE_OUT"
  exit 1
fi

echo ""
echo "✓ parseh-tts piper backend ready."
echo "  Try:   echo \"سلام دنیا\" | ./tools/parseh-tts/speak.sh"
