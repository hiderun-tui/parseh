# parseh-tts — local-only Persian text-to-speech

A small CLI utility for the PARSEH project: takes Persian (or English) text and produces speech audio, using only local inference. **Zero external network traffic at runtime.** Matches the PARSEH "no telemetry, no external egress" rule.

## Why this exists

Two reasons:

1. **Maintainer ergonomics today.** PARSEH conversations sometimes happen in Persian. Reading long Persian responses on a terminal is harder than hearing them spoken. This tool lets responses be played back audibly.
2. **Future Hiderun accessibility.** The Hiderun client will eventually need TTS for accessibility (blind users, voice-message readback). Having a clean local TTS wrapper in the repo means that capability has a home before it's needed.

## Engines

The wrapper tries three options in order:

| Engine | Quality | Install size | Install steps |
|---|---|---|---|
| **piper-tts** (recommended) | High — natural-sounding | ~100 MB (binary + 1 voice model) | `./install-piper.sh` |
| **espeak-ng** (fallback) | Low — robotic but legible | ~5 MB | `sudo apt install -y espeak-ng` |
| File output (last resort) | n/a | n/a | writes text to `/tmp/parseh-tts-*.txt` and prints install hints |

Both engines run **fully offline**. No cloud TTS API is ever invoked. The piper installer downloads the binary + voice **once** from rhasspy's GitHub releases + Hugging Face mirror (verified by SHA-256 once the hash is pinned).

## Quick install

```bash
# Best quality (recommended):
./tools/parseh-tts/install-piper.sh

# Or lightweight fallback:
sudo apt install -y espeak-ng
```

## Usage

```bash
# From stdin:
echo "سلام دنیا" | ./tools/parseh-tts/speak.sh

# As an argument:
./tools/parseh-tts/speak.sh "خوش آمدید به پارسه"

# English (uses the same piper Persian voice if no English voice installed —
# results will be poor; install an English voice for English work):
./tools/parseh-tts/speak.sh --lang en "hello world"

# Save to file instead of playing:
./tools/parseh-tts/speak.sh --save out.wav "متن آزمایشی"

# Pick a different Persian voice:
./tools/parseh-tts/speak.sh --voice fa_IR-ganji-medium "نمونه صدای دیگر"
```

## Available piper Persian voices

From [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices) under `fa/`:

| Voice name | Gender | Quality | Notes |
|---|---|---|---|
| `fa_IR-amir-medium` | male | medium | **default** — clean, neutral |
| `fa_IR-ganji-medium` | male | medium | warmer tone |
| `fa_IR-ganji_adabi-medium` | male | medium | more literary register |
| `fa_IR-gyro-medium` | male | medium | alternative voice |

Install additional voices by re-running `install-piper.sh --voice <voice-name>`.

## Audio playback

The wrapper auto-detects an audio player in this order:

1. `paplay` (PulseAudio — works in WSLg)
2. `aplay` (ALSA)
3. `mpv`
4. `ffplay`
5. `play` (SoX)

Override with `PARSEH_TTS_PLAYER=mpv ./speak.sh ...` if needed.

**WSL note:** PulseAudio audio works under WSLg out of the box on Windows 11. On older WSL versions you may need to configure PulseAudio bridge. If audio doesn't play, use `--save out.wav` and open the file in a Windows player.

## File layout

```
tools/parseh-tts/
├── README.md           — this file
├── speak.sh            — main wrapper (run this)
├── install-piper.sh    — one-time piper + voice install
└── bin/                — symlinks to installed piper binary (.gitignored)

$HOME/.parseh/tts/
├── bin/piper           — actual piper binary
└── voices/             — downloaded .onnx + .onnx.json voice files
```

Generated/downloaded artefacts under `$HOME/.parseh/` are **not** committed to the repo. Each developer installs once on their own machine.

## Privacy + security

- Both engines run **locally**. No request is sent to any external service when speaking text.
- The `install-piper.sh` script makes **two** external downloads (the piper binary + a voice model) on first run from `github.com/rhasspy/piper` and `huggingface.co/rhasspy/piper-voices`. These are public, audited, MIT-licensed sources. SHA-256 verification is wired in but the first-install hash is a `PLACEHOLDER` — the user must replace it on their first install (the installer prints the observed hash).
- No telemetry is added. No logs leave the machine.
- The voice model has no network capabilities; it's a static ONNX file inferred locally.

## License

Apache-2.0 (matching PARSEH). piper itself is MIT.

## Future work

- English voice download path (V0.3+)
- Cached voice-model SHA-256 hashes pinned (V0.2 follow-up — currently `PLACEHOLDER`)
- Integration hook so PARSEH miner logs in Persian can be optionally spoken (debug ergonomics)
- A Tauri-accessible binding for the Hiderun client to enable in-app TTS read-back (V1+)
