#!/usr/bin/env bash
# parseh-stt/listen.sh — local-only Persian (and multilingual) speech-to-text.
#
# Records audio from microphone OR transcribes an audio file, runs it through
# whisper.cpp with a multilingual model (default: small), and prints the
# transcription to stdout.
#
# Privacy: ZERO external network traffic at runtime. Transcription happens
# fully locally on the user's machine. Matches the PARSEH "no telemetry,
# no external egress" rule.
#
# Usage:
#   ./tools/parseh-stt/listen.sh               # record from mic, transcribe
#   ./tools/parseh-stt/listen.sh --seconds 30  # record for 30s instead of default 10
#   ./tools/parseh-stt/listen.sh in.wav        # transcribe an existing file
#   ./tools/parseh-stt/listen.sh --lang fa in.wav    # explicit language
#   ./tools/parseh-stt/listen.sh --lang auto in.wav  # auto-detect
#   ./tools/parseh-stt/listen.sh --save out.txt in.wav  # write to file
#
# Env vars:
#   PARSEH_STT_MODEL    model name (default: small)
#   PARSEH_STT_DIR      $HOME/.parseh/stt (model + binary location)
#   PARSEH_STT_RECORDER override recording tool (default: auto-detect)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_MODEL="${PARSEH_STT_MODEL:-small}"
PARSEH_STT_DIR="${PARSEH_STT_DIR:-$HOME/.parseh/stt}"
BIN_DIR="$PARSEH_STT_DIR/bin"
MODELS_DIR="$PARSEH_STT_DIR/models"

LANG_MODE="fa"
SAVE_PATH=""
SECONDS_REC=10
MODEL="$DEFAULT_MODEL"
AUDIO_FILE=""

# ---- parse args ----
while [[ $# -gt 0 ]]; do
  case "$1" in
    --lang)    LANG_MODE="$2"; shift 2 ;;
    --model)   MODEL="$2"; shift 2 ;;
    --save)    SAVE_PATH="$2"; shift 2 ;;
    --seconds) SECONDS_REC="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,23p' "$0"
      exit 0 ;;
    -*)
      echo "ERROR: unknown flag: $1" >&2
      exit 2 ;;
    *) AUDIO_FILE="$1"; shift ;;
  esac
done

# ---- locate whisper-cli binary ----
detect_whisper() {
  if [[ -x "$SCRIPT_DIR/bin/whisper-cli" ]]; then
    echo "$SCRIPT_DIR/bin/whisper-cli"; return 0
  fi
  if [[ -x "$BIN_DIR/whisper-cli" ]]; then
    echo "$BIN_DIR/whisper-cli"; return 0
  fi
  # Legacy whisper.cpp naming: "main"
  if [[ -x "$SCRIPT_DIR/bin/main" ]]; then
    echo "$SCRIPT_DIR/bin/main"; return 0
  fi
  if [[ -x "$BIN_DIR/main" ]]; then
    echo "$BIN_DIR/main"; return 0
  fi
  if command -v whisper-cli >/dev/null 2>&1; then
    command -v whisper-cli; return 0
  fi
  return 1
}

# ---- locate model file ----
detect_model() {
  local model_file="$MODELS_DIR/ggml-${MODEL}.bin"
  if [[ -f "$model_file" ]]; then
    echo "$model_file"; return 0
  fi
  return 1
}

# ---- pick a recording tool ----
detect_recorder() {
  if [[ -n "${PARSEH_STT_RECORDER:-}" ]]; then echo "$PARSEH_STT_RECORDER"; return; fi
  # arecord (ALSA) is preferred — most reliable for raw 16k mono WAV
  for r in arecord parecord sox ffmpeg; do
    if command -v "$r" >/dev/null 2>&1; then echo "$r"; return; fi
  done
  echo ""
}

# ---- record audio to a temp WAV ----
record_audio() {
  local out_wav="$1"
  local seconds="$2"
  local recorder; recorder="$(detect_recorder)"
  if [[ -z "$recorder" ]]; then
    echo "ERROR: no recording tool found (arecord/parecord/sox/ffmpeg)." >&2
    echo "       install one of them, or pass an existing WAV file as argument." >&2
    return 1
  fi

  echo "→ recording ${seconds}s of audio via $recorder ..." >&2
  case "$recorder" in
    arecord)
      arecord -q -f S16_LE -r 16000 -c 1 -d "$seconds" "$out_wav" 2>/dev/null
      ;;
    parecord)
      # parecord (PulseAudio) — 16kHz mono signed-16 little-endian
      parecord --rate=16000 --channels=1 --format=s16le \
        --file-format=wav "$out_wav" >/dev/null 2>&1 &
      local pid=$!
      sleep "$seconds"
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      ;;
    sox)
      # sox: record to 16kHz mono signed-16 WAV
      sox -q -d -r 16000 -c 1 -b 16 "$out_wav" trim 0 "$seconds" 2>/dev/null
      ;;
    ffmpeg)
      # ffmpeg fallback — try ALSA default, then PulseAudio default.
      # Forces 16kHz mono PCM so whisper.cpp doesn't need to resample.
      if ! ffmpeg -hide_banner -loglevel error -y \
            -f alsa -i default -t "$seconds" \
            -ar 16000 -ac 1 -sample_fmt s16 "$out_wav" 2>/dev/null; then
        ffmpeg -hide_banner -loglevel error -y \
          -f pulse -i default -t "$seconds" \
          -ar 16000 -ac 1 -sample_fmt s16 "$out_wav" 2>/dev/null
      fi
      ;;
  esac

  if [[ ! -s "$out_wav" ]]; then
    echo "ERROR: recording produced no audio (file empty)." >&2
    return 1
  fi
  return 0
}

# ---- run whisper.cpp on a WAV file, print plain text to stdout ----
run_whisper() {
  local wav="$1"
  local whisper_bin="$2"
  local model_file="$3"

  # whisper.cpp writes the transcription to <wav>.txt when -otxt is set.
  # We use -nt (no timestamps), -np (no progress), -l <lang>.
  local tmp_prefix; tmp_prefix="$(mktemp -u)"
  cp "$wav" "${tmp_prefix}.wav"

  if ! "$whisper_bin" \
        --model "$model_file" \
        --language "$LANG_MODE" \
        --no-timestamps \
        --no-prints \
        --output-txt \
        --output-file "$tmp_prefix" \
        --file "${tmp_prefix}.wav" >/dev/null 2>&1; then
    # Older whisper.cpp builds use different flag spellings — retry with short flags.
    if ! "$whisper_bin" \
          -m "$model_file" \
          -l "$LANG_MODE" \
          -nt \
          -np \
          -otxt \
          -of "$tmp_prefix" \
          -f "${tmp_prefix}.wav" >/dev/null 2>&1; then
      rm -f "${tmp_prefix}.wav" "${tmp_prefix}.txt"
      return 1
    fi
  fi

  if [[ -f "${tmp_prefix}.txt" ]]; then
    # Trim trailing whitespace per line; strip the trailing blank.
    sed -e 's/[[:space:]]*$//' "${tmp_prefix}.txt"
    rm -f "${tmp_prefix}.wav" "${tmp_prefix}.txt"
    return 0
  fi

  rm -f "${tmp_prefix}.wav" "${tmp_prefix}.txt"
  return 1
}

# ---- fallback message ----
fallback_no_engine() {
  cat >&2 <<EOF

No STT engine installed. whisper.cpp is not yet set up on this machine.

To enable speech-to-text:

  ./tools/parseh-stt/install-whisper.sh

This installs whisper.cpp (~5 MB binary) + the multilingual 'small' model
(~466 MB) into \$HOME/.parseh/stt/. All inference is local — no cloud API.

For a faster but lower-quality model:
  ./tools/parseh-stt/install-whisper.sh --model tiny   # ~75 MB

After install, re-run this script.
EOF
  exit 3
}

# ---- main ----
WHISPER_BIN="$(detect_whisper)" || fallback_no_engine
MODEL_FILE="$(detect_model)" || {
  echo "INFO: model '$MODEL' not found at $MODELS_DIR/ggml-${MODEL}.bin" >&2
  echo "      run ./tools/parseh-stt/install-whisper.sh --model $MODEL" >&2
  fallback_no_engine
}

# Determine input audio source
if [[ -n "$AUDIO_FILE" ]]; then
  if [[ ! -f "$AUDIO_FILE" ]]; then
    echo "ERROR: audio file not found: $AUDIO_FILE" >&2
    exit 2
  fi
  INPUT_WAV="$AUDIO_FILE"
  CLEANUP_WAV=""
else
  INPUT_WAV="$(mktemp --suffix=.wav)"
  CLEANUP_WAV="$INPUT_WAV"
  if ! record_audio "$INPUT_WAV" "$SECONDS_REC"; then
    rm -f "$CLEANUP_WAV"
    exit 1
  fi
fi

# Transcribe
if TEXT="$(run_whisper "$INPUT_WAV" "$WHISPER_BIN" "$MODEL_FILE")"; then
  [[ -n "$CLEANUP_WAV" ]] && rm -f "$CLEANUP_WAV"
  if [[ -n "$SAVE_PATH" ]]; then
    printf '%s\n' "$TEXT" > "$SAVE_PATH"
    echo "saved: $SAVE_PATH" >&2
  else
    printf '%s\n' "$TEXT"
  fi
  exit 0
else
  [[ -n "$CLEANUP_WAV" ]] && rm -f "$CLEANUP_WAV"
  echo "ERROR: whisper-cli failed to produce transcription." >&2
  exit 1
fi
