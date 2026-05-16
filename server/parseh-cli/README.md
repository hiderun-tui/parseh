# parseh-cli

Cross-platform developer CLI for PARSEH. Users type `parseh` in their
terminal (Linux / macOS / Windows) and get a clap-derived menu of
subcommands for: submitting test jobs, querying the local network
state, running protocol acceptance tests, and reporting issues for
OSS-contributor fix-up.

This binary is **developer ergonomics — not protocol semantics, not
economic features.**

## What this CLI deliberately does NOT do (and why)

The user's original request for this batch included two items that were
deliberately refused, with the refusal documented in writing here per
the cultural rule in
the project notes:

- **Agent marketplace / investment / trading surface.** Refused: V0.2
  has just crossed the protocol-formation threshold; the binding
  maintainer note for this period is "do NOT immediately jump to
  token launch, public mining, exchange discussion, large-scale
  onboarding." A marketplace surface is precisely that jump.
- **"Better-agent-earns-more-PARSEH" PoW.** Refused: there is no
  PARSEH chain in V0.2 (deferred to V0.3+ per `chain-spec.md`), so
  there is nothing for an earnings primitive to settle into. Adding
  the surface now would either be a stub that lies to the user or a
  premature economic experiment.

If those features are needed in V0.3+, they belong in a separate crate
behind an explicit feature flag, with their own RFC. They do not belong
in the developer-ergonomics CLI.

## Install

From a checkout:

```
cargo install --path server/parseh-cli
```

From a pre-built release binary (when V0.2 releases ship):

```
# Linux/macOS
curl -L https://github.com/hiderun-tui/parseh/releases/latest/download/parseh-x86_64-linux \
  -o /usr/local/bin/parseh && chmod +x /usr/local/bin/parseh

# Windows portable ZIP includes parseh.exe alongside parseh-miner.exe
```

The Windows portable build is produced by:

```
the release build script
```

## Subcommands

Every subcommand has its own `--help` with examples. Global flags:

- `--db PATH` (`$PARSEH_DB`) — override SharedState DB path
  (default: `$HOME/.parseh/shared-state.db`)
- `--identity PATH` (`$PARSEH_IDENTITY`) — override identity file
  (default: `$HOME/.config/parseh/identity.ed25519`)
- `-v / -vv / -vvv` — increase log verbosity (stderr)

### `parseh`

Prints an overview + suggestions when run with no subcommand.

### `parseh status [--text]`

Network + local-state summary. JSON by default for scripting; `--text`
for human-readable.

```
$ parseh status --text
parseh-cli  : 0.1.0-dev
peer_id     : 12D3KooW...
config_dir  : /home/me/.config/parseh
identity    : /home/me/.config/parseh/identity.ed25519

shared-state
  path           : /home/me/.parseh/shared-state.db
  exists         : false
  schema_version : 1
  tasks          : 0
  outcomes       : 0
  reputation_log : 0
  established_peers: 0

miner_running   : false
llm_runtime     : (probe failed or none installed)
```

### `parseh detect [--text]`

Probes local LLM runtimes (Ollama, llama.cpp, GGUF, GPU). Wraps
`parseh-llm-detect`.

### `parseh submit "<prompt>" [--seed N] [--sensitive] [--speak]`

Builds + signs + serialises a `parseh_task::JobSpec`. V0.2 prints the
signed CBOR-hex and content hash; V0.3+ wiring will push it onto a
running miner's request-response stream. `--speak` shells out to
`an optional local Persian TTS helper` to read the prompt aloud (Persian default).

```
$ parseh submit "What is consensus?"
{
  "task_id": "a8b9...",
  "spec_cbor_hex": "a8...",
  "submitter": "12D3KooW...",
  "sensitive": false,
  "bytes": 162,
  "note": "V0.2: signed offline · network submission via parseh-miner request-response is V0.3+ wiring"
}

$ parseh submit --file ./prompt.txt
$ parseh submit --seed 42 "deterministic"
```

### `parseh tail [--interval-ms 1000] [--max 0]`

Polls SharedState every `interval_ms` and prints each new state-delta
as one JSON line. Exits on Ctrl-C.

### `parseh test [--acceptance] [--report]`

- `parseh test` — `cargo test --workspace --release` from the repo
  root (auto-detected by walking up looking for a workspace `Cargo.toml`).
- `parseh test --acceptance` — `cargo test -p parseh-testnet --release
  -- --nocapture --test-threads=1`. On PASS, prints the V0.2
  threshold framing from
  the project notes
  ("functioning distributed coordination primitive · NOT
  production-ready · NOT censorship-resistant · NOT economically
  hardened · but REAL"). On FAIL, prints the same note's FAIL framing
  ("valuable diagnostic data") plus the last 30 lines of test output.
- `parseh test --report` — runs both, captures all output, writes
  `<temp>/parseh-test-report-<UTC>.md`, and tells the user
  `parseh report-issue --attach …`. The temp dir is `std::env::temp_dir()`
  so this works on Windows (`%TEMP%`) and Linux (`/tmp`) without
  hard-coding `/tmp`.

### `parseh report-issue [--attach PATH] [--title "..."] [--dry-run]`

Builds a markdown issue body with: env info, local `peer_id`, last 50
log lines (if found at `~/.parseh/miner.log`), and a steps-to-reproduce
template. Then:

- if `gh` is on PATH → invokes `gh issue create --body-file …`.
- otherwise → prints the markdown and exits 0.

### `parseh peers [--filter inference]`

Lists peers stored in the SharedState reputation log with their summed
reputation. The capability column is empty in V0.2 — capability ads live
in the miner's in-memory `PeerRegistry`; once the miner persists them
this fills out automatically.

### `parseh whoami`

Prints `peer_id`, identity path, summed reputation, and whether the
identity was just generated. Generates a fresh identity at the default
path if none exists.

### `parseh tts "<text>" [--lang fa|en]`

Shells out to `an optional local Persian TTS helper` (which auto-detects
piper-tts or espeak-ng locally). Exits with code 3 if the wrapper is
not present.

### `parseh stt [--seconds N]`

Shells out to `an optional local STT helper (V0.3+)` (V0.3+ — wrapper does not
exist yet). Exits 3 with an informative message until it lands.

## Integration with other tools

- **`parseh-tts`** — `parseh tts` shells out to the existing wrapper at
  `an optional local Persian TTS helper`. Privacy: the wrapper uses local
  piper/espeak-ng; no network egress.
- **`parseh-stt`** — `parseh stt` looks for
  `an optional local STT helper (V0.3+)` and falls back gracefully when missing.
- **`gh`** — `parseh report-issue` uses the GitHub CLI when present,
  with a markdown-print fallback for systems where `gh` is not yet
  installed.

## SharedState encryption

`parseh-cli` opens the same SQLCipher-encrypted SharedState DB the
miner uses. The key is derived by SHA-256 of the local
`identity.ed25519` file (per
`parseh_shared_state::KeySource::IdentityFile`). This is the V0.2
recommended dev path; the file's caveat applies — anyone with read
access to your identity file can also decrypt the DB. V0.3+ adds a
passphrase UI; until then, treat both files with the same care as a
secret key.

## Exit codes

| Code | Meaning                                                       |
| ---- | ------------------------------------------------------------- |
| 0    | Success.                                                      |
| 1    | Generic error (anything not specifically classified below).   |
| 2    | A required DB / file is missing.                              |
| 3    | An optional external tool (`parseh-tts`, `parseh-stt`) is not installed. |
| 4    | `parseh test --acceptance` ran but the acceptance test FAILED. |

## License

Apache-2.0 — same as the rest of PARSEH.
