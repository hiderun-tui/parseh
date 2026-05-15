#!/usr/bin/env bash
# parseh-tts/speak.sh — local-only Persian (and English) text-to-speech wrapper.
#
# Reads text from stdin or from arguments, generates a WAV via a local TTS
# engine, and plays it. Detection order:
#   1. piper-tts  — preferred (high quality, offline, ~100MB install)
#   2. espeak-ng  — fallback (lower quality, ~5MB apt install, works without
#                  downloaded voice models)
#   3. no engine  — writes the text to a temp file and tells the user to install.
#
# Privacy: ZERO external network traffic at runtime. Both piper and
# espeak-ng do all inference locally. No cloud TTS APIs are ever invoked.
# This matches the PARSEH "no telemetry, no external egress" rule.
#
# Usage:
#   echo "سلام دنیا"     | ./tools/parseh-tts/speak.sh
#   ./tools/parseh-tts/speak.sh "سلام دنیا"
#   ./tools/parseh-tts/speak.sh --lang en "hello world"
#   ./tools/parseh-tts/speak.sh --voice fa_IR-ganji-medium "متن"
#   ./tools/parseh-tts/speak.sh --save out.wav "متن"   # don't play, just save
#
# Env vars:
#   PARSEH_TTS_VOICE       voice model name (default: fa_IR-amir-medium)
#   PARSEH_TTS_VOICES_DIR  where piper voices live (default: ~/.parseh/tts/voices)
#   PARSEH_TTS_PLAYER      override audio player (default: auto-detect)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_VOICE="${PARSEH_TTS_VOICE:-fa_IR-amir-medium}"
VOICES_DIR="${PARSEH_TTS_VOICES_DIR:-$HOME/.parseh/tts/voices}"
LANG_MODE="fa"
SAVE_PATH=""
VOICE="$DEFAULT_VOICE"

# ---- parse args ----
while [[ $# -gt 0 ]]; do
  case "$1" in
    --lang)   LANG_MODE="$2"; shift 2 ;;
    --voice)  VOICE="$2"; shift 2 ;;
    --save)   SAVE_PATH="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,28p' "$0"
      exit 0 ;;
    *) break ;;
  esac
done

# ---- collect text (args take precedence over stdin) ----
if [[ $# -gt 0 ]]; then
  TEXT="$*"
elif ! [ -t 0 ]; then
  TEXT="$(cat)"
else
  echo "ERROR: no text given. Pass as arg or pipe via stdin." >&2
  exit 2
fi

if [[ -z "${TEXT// /}" ]]; then
  echo "ERROR: empty input" >&2
  exit 2
fi

# ---- pick an audio player ----
detect_player() {
  if [[ -n "${PARSEH_TTS_PLAYER:-}" ]]; then echo "$PARSEH_TTS_PLAYER"; return; fi
  for p in paplay aplay mpv ffplay play; do
    if command -v "$p" >/dev/null 2>&1; then echo "$p"; return; fi
  done
  echo ""
}

play_wav() {
  local wav="$1"
  local player; player="$(detect_player)"
  if [[ -z "$player" ]]; then
    echo "WARN: no audio player found (paplay/aplay/mpv/ffplay/play). WAV saved at $wav" >&2
    return 0
  fi
  case "$player" in
    paplay|aplay|play) "$player" "$wav" 2>/dev/null ;;
    mpv)               mpv --really-quiet "$wav" 2>/dev/null ;;
    ffplay)            ffplay -autoexit -nodisp -loglevel quiet "$wav" ;;
  esac
}

# ---- piper path (preferred) ----
try_piper() {
  local piper_bin=""
  if [[ -x "$SCRIPT_DIR/bin/piper" ]]; then
    piper_bin="$SCRIPT_DIR/bin/piper"
  elif command -v piper >/dev/null 2>&1; then
    piper_bin="piper"
  elif command -v piper-tts >/dev/null 2>&1; then
    piper_bin="piper-tts"
  else
    return 1
  fi

  # Default voice file location
  local voice_onnx="$VOICES_DIR/$VOICE.onnx"
  local voice_json="$VOICES_DIR/$VOICE.onnx.json"
  if [[ ! -f "$voice_onnx" ]]; then
    echo "INFO: piper voice not found at $voice_onnx" >&2
    echo "      run ./tools/parseh-tts/install-piper.sh to fetch '$VOICE'" >&2
    return 1
  fi

  local out_wav; out_wav="${SAVE_PATH:-$(mktemp --suffix=.wav)}"
  if printf '%s\n' "$TEXT" | "$piper_bin" --model "$voice_onnx" --output_file "$out_wav" >/dev/null 2>&1; then
    if [[ -z "$SAVE_PATH" ]]; then
      play_wav "$out_wav"
      rm -f "$out_wav"
    else
      echo "saved: $out_wav"
    fi
    return 0
  else
    return 1
  fi
}

# ---- espeak-ng path (fallback) ----
try_espeak() {
  if ! command -v espeak-ng >/dev/null 2>&1; then
    return 1
  fi
  # Map language to espeak voice
  local espeak_voice
  case "$LANG_MODE" in
    fa) espeak_voice="fa" ;;
    en) espeak_voice="en-us" ;;
    *)  espeak_voice="$LANG_MODE" ;;
  esac

  local out_wav; out_wav="${SAVE_PATH:-$(mktemp --suffix=.wav)}"
  if printf '%s\n' "$TEXT" | espeak-ng -v "$espeak_voice" -w "$out_wav" >/dev/null 2>&1; then
    if [[ -z "$SAVE_PATH" ]]; then
      play_wav "$out_wav"
      rm -f "$out_wav"
    else
      echo "saved: $out_wav"
    fi
    return 0
  else
    return 1
  fi
}

# ---- last resort: write text to /tmp and instruct ----
fallback_no_engine() {
  local out="/tmp/parseh-tts-$$.txt"
  printf '%s\n' "$TEXT" > "$out"
  cat >&2 <<EOF

No TTS engine installed. Text saved to: $out

To enable speech:

  Option A (best quality, ~100 MB):
    ./tools/parseh-tts/install-piper.sh

  Option B (lighter, lower quality):
    sudo apt install -y espeak-ng

After install, re-run this script.
EOF
  exit 3
}

# ---- main ----
if try_piper; then exit 0; fi
if try_espeak; then exit 0; fi
fallback_no_engine
