# parseh-stt — local-only Persian speech-to-text

A small CLI utility for the PARSEH project: takes a Persian (or other-language) audio recording — either captured live from the microphone or loaded from a file — and produces a text transcription. **Zero external network traffic at runtime.** Matches the PARSEH "no telemetry, no external egress" rule.

## Why this exists

Two reasons:

1. **Persian-speaking contributor onboarding.** Many would-be PARSEH contributors are more fluent dictating in Persian than typing. A local STT wrapper lets them speak a bug report, design idea, or commit message and turn it into text — without sending audio to any cloud service.
2. **V0.2 "speak-your-idea → JobSpec" flow.** The forthcoming PARSEH CLI will let a node operator speak a task or job, have it transcribed locally, then routed into the coordination layer as a JobSpec. This tool is the foundation for that path. Today it is a standalone wrapper; once `parseh-cli` lands, it becomes a building block.

## Engine choice

The wrapper uses **[whisper.cpp](https://github.com/ggerganov/whisper.cpp)** with one of the multilingual Whisper models. Rationale:

- **C++ implementation** — no Python runtime needed; static binary releases
- **Multilingual models** — `tiny / base / small / medium / large-v3` all support Persian (`fa`)
- **Small model** (~466 MB) runs at roughly real-time on a modern CPU and gives respectable Persian accuracy
- **Fully offline** once installed — no cloud API, no telemetry

We deliberately **do not** integrate the OpenAI Whisper API (cloud — violates the no-egress rule) or `faster-whisper` (Python — heavier install footprint for a CLI helper).

## Quick install

```bash
# Default (recommended): small multilingual model, ~466 MB total install
./tools/parseh-stt/install-whisper.sh

# Smaller + faster (less accurate Persian):
./tools/parseh-stt/install-whisper.sh --model tiny

# Larger + more accurate (slower):
./tools/parseh-stt/install-whisper.sh --model medium
```

## Usage

```bash
# Record 10 seconds from the microphone, transcribe to stdout (Persian):
./tools/parseh-stt/listen.sh

# Record 30 seconds instead:
./tools/parseh-stt/listen.sh --seconds 30

# Transcribe an existing WAV file:
./tools/parseh-stt/listen.sh my-recording.wav

# Auto-detect language (use for non-Persian inputs):
./tools/parseh-stt/listen.sh --lang auto english-clip.wav

# Force English:
./tools/parseh-stt/listen.sh --lang en english-clip.wav

# Save transcript to a file instead of stdout:
./tools/parseh-stt/listen.sh --save out.txt my-recording.wav
```

Persian (`fa`) is the default language because PARSEH's primary user base is Persian-speaking. Set `--lang auto` to let Whisper detect, or `--lang en` (or any [ISO 639-1 code](https://en.wikipedia.org/wiki/List_of_ISO_639-1_codes) Whisper supports) for explicit override.

## Available models

From [huggingface.co/ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp):

| Model name | Size | Approx. speed (CPU) | Persian accuracy |
|---|---|---|---|
| `tiny` | ~75 MB | ~10× real-time | Marginal — recognisable words but many errors |
| `base` | ~142 MB | ~7× real-time | Usable for short, clean clips |
| `small` | ~466 MB | ~2× real-time | **Default** — good for ordinary dictation |
| `medium` | ~1.5 GB | ~0.5× real-time | Clear improvement on dialect + noise |
| `large-v3` | ~3 GB | ~0.2× real-time | Best — overkill for short clips |

We do **not** ship the `.en` model variants — they are English-only and cannot transcribe Persian. The installer rejects them.

Install additional models by re-running `install-whisper.sh --model <name>`; switch between installed models at call time via `PARSEH_STT_MODEL=tiny ./listen.sh ...` or `--model tiny`.

## Audio recording

When called with no audio-file argument, the wrapper auto-detects a recording tool in this order:

1. `arecord` (ALSA) — preferred, most reliable for raw 16 kHz mono WAV
2. `parecord` (PulseAudio) — works in WSLg
3. `sox` — cross-platform, sometimes needed on macOS
4. `ffmpeg` — last resort; tries ALSA then PulseAudio, forces 16 kHz mono PCM so whisper.cpp doesn't have to resample

Override with `PARSEH_STT_RECORDER=ffmpeg ./listen.sh ...` if auto-detect picks the wrong tool.

If none of those are installed, the wrapper exits with a friendly error and the install hint. You can always pre-record a WAV file by other means and pass it as an argument.

**WSL note:** WSLg microphone access is brittle on Windows 10 and early Windows 11 builds. The reliable workflow is:

1. Record on the Windows side (e.g., Voice Recorder, ffmpeg-for-Windows, or any DAW)
2. Export as 16 kHz mono WAV
3. Run `./tools/parseh-stt/listen.sh /mnt/c/path/to/recording.wav` from WSL

PulseAudio capture under WSLg may work depending on your WSL version + the Windows mic privacy settings.

## File layout

```
tools/parseh-stt/
├── README.md             — this file
├── listen.sh             — main wrapper (run this)
├── install-whisper.sh    — one-time whisper.cpp + model install
├── bin/                  — symlink to installed whisper-cli (.gitignored)
└── models/               — gitignored — not used in-tree (kept in $HOME)

$HOME/.parseh/stt/
├── bin/whisper-cli       — actual whisper.cpp binary (+ shared libs)
└── models/               — downloaded ggml-*.bin files
```

Generated/downloaded artefacts under `$HOME/.parseh/` are **not** committed to the repo. Each developer installs once on their own machine.

## Privacy + security

- whisper.cpp runs **locally**. No request is sent to any external service when transcribing.
- The `install-whisper.sh` script makes **two** external downloads on first run from `github.com/ggerganov/whisper.cpp` and `huggingface.co/ggerganov/whisper.cpp`. These are public, audited, MIT-licensed sources. SHA-256 verification is wired in but the first-install hash is a `PLACEHOLDER` — the user must replace it on their first install (the installer prints the observed hash).
- No telemetry is added. No logs leave the machine. The transcript stays on the user's disk (stdout, or `--save` path).
- The model file has no network capabilities; it's a static GGML weights file inferred locally.
- This is the **counterpart** to [`tools/parseh-tts/`](../parseh-tts/) — same privacy contract, same install pattern, same gitignored install location.

## License

Apache-2.0 (matching PARSEH). whisper.cpp itself is MIT.

## Future work

- `parseh-cli speak-your-idea` end-to-end flow: record → transcribe → produce JobSpec JSON → submit to coordination layer (V0.3+)
- Voice activity detection (VAD) so `listen.sh` stops automatically when the speaker pauses, instead of needing `--seconds`
- Streaming mode (whisper.cpp's `stream` example) for live captioning
- Cached model SHA-256 hashes pinned (V0.2 follow-up — currently `PLACEHOLDER`)
- A Tauri-accessible binding for the Hiderun client to enable in-app voice notes (V1+)
