# parseh-coord — operator community notification + messaging + issue broadcast

A standalone Rust binary for the **operator** (one human + Claude Code) to
keep up with community activity across platforms, draft replies, and
broadcast "help wanted" issues — **with a human in the loop on every send**.

> **PARSEH is v0.1.0-alpha. The network is NOT operational yet** (zero
> bootstrap servers provisioned). This tool is operator/community plumbing,
> not part of the network. It is deliberately **not** a member of the
> `server/` Rust workspace — it has its own `Cargo.toml` and `Cargo.lock`,
> exactly like `tools/parseh-mcp`, so it never touches the
> bootstrap-blocked network missions.

## What it is (and is not)

- **Is:** a local CLI that ingests GitHub issues/comments/discussion
  comments into a local SQLite store, lets the operator (with Claude Code)
  triage an inbox, draft replies, and explicitly approve + send them, and
  bulk-create contribution issues from a file.
- **Is NOT:** an autonomous daemon. **Nothing is ever posted to a human
  automatically.** Every outbound message must be explicitly `draft`ed,
  explicitly `approve`d, and explicitly `send`. `send` refuses any entry
  whose status is not exactly `approved`. This is enforced in code (twice:
  in the CLI `send` path and again in `Store::mark_sent`) and is
  non-negotiable.

### Connector scope (honest)

| Connector | State | Notes |
|---|---|---|
| **GitHub** | **REAL** | Issues + issue comments (REST) + discussion comments (GraphQL); posts issue comments; creates issues. |
| **Codeberg** | **REAL** | Forgejo REST (`https://codeberg.org/api/v1`). Polls open issues + their comments; posts issue comments; creates issues (without labels — see Limitations). Forgejo auth is `Authorization: token <TOKEN>`. Network calls untested offline; delivery depends on Codeberg availability. |
| **Discord** | **REAL** | Discord REST (`https://discord.com/api/v10`), **polling, not the realtime gateway**. Reads recent messages from configured channels; posts messages. Messaging only — deliberately **not** in `broadcast-issues` (issues are a GitHub/Codeberg concept). Bot auth is `Authorization: Bot <TOKEN>`. Bot/self messages are skipped. Network calls untested offline; delivery depends on Discord availability. |
| **Nostr** | **REAL** | Polls mentions + a hashtag; posts kind-1 replies / top-level notes; NIP-23 long-form. Posting is **best-effort across public, unmoderated relays** — not guaranteed, not anonymous, not "uncensorable". A relay can drop/delay/refuse an event; failures are surfaced honestly, never faked. |
| **Matrix** | **STUB** | Trait impl that returns a clear `bail!`. Not faked. Wiring it later is additive. |

## Build

```bash
cd tools/parseh-coord
cargo build --release
cargo test          # 47 tests, all offline (no network in tests)
```

## Credentials — never from the repo

Credentials are read from the **environment only**, or from
`~/.parseh/coord-creds.toml` (which is loaded into the process env at
startup; real env vars always win). The credentials file lives **outside
the repo**. `~/.parseh/` and the relevant filenames are gitignored at the
repo root as a safety net.

Environment variables:

- `PARSEH_COORD_GITHUB_TOKEN` — **required** for any GitHub operation. A PAT
  (classic or fine-grained) with `repo` scope (or `public_repo` +
  `discussions` for read-only ingest of a public repo; issue creation /
  commenting needs write). If absent, `poll`/`post`/`broadcast-issues` fail
  with a clear, friendly error naming the variable — never a panic.
- `PARSEH_COORD_GITHUB_REPO` — `owner/name`. Default `hiderun-tui/parseh`.

Codeberg environment variables (same env-over-file precedence as GitHub):

- `PARSEH_COORD_CODEBERG_TOKEN` — **required** for any Codeberg operation.
  A Forgejo access token with repo issue read/write scope. If absent,
  `poll`/`post`/`broadcast-issues --platform codeberg` fail with a clear,
  friendly error naming the variable — never a panic.
- `PARSEH_COORD_CODEBERG_REPO` — `owner/name`. **Required, no default** —
  the Codeberg mirror is not a fixed known location, so guessing one would
  be dishonest. If absent, operations fail with a friendly error naming the
  variable. If the configured repo does not exist on Codeberg (HTTP 404),
  the error is actionable ("create/migrate it there first") — never a
  panic, and parseh-coord never creates the repository for you.

Discord environment variables (same env-over-file precedence as GitHub):

- `PARSEH_COORD_DISCORD_TOKEN` — **required** for any Discord operation.
  A Discord **bot** token (the value sent as `Authorization: Bot <TOKEN>`).
  Never logged. If absent, `poll`/`post` fail with a clear, friendly error
  naming the variable — never a panic.
- `PARSEH_COORD_DISCORD_CHANNELS` — comma-separated channel IDs to poll.
  **Required, no default** — the project has no fixed Discord channel, so
  guessing one would be dishonest. If absent, `poll` fails with a friendly
  error naming the variable — never a panic.

Operator setup prerequisites (Discord):

- Create a bot application in the Discord Developer Portal and copy its
  **bot token** into `PARSEH_COORD_DISCORD_TOKEN` (env or creds file only).
- **Enable the "Message Content Intent"** for the bot (Developer Portal →
  your app → Bot → Privileged Gateway Intents). Without it, `content` comes
  back empty and polled messages have no body.
- **Invite the bot** to the server with the **View Channels**, **Send
  Messages**, and **Read Message History** permissions.
- Put the target **channel IDs** (Discord → enable Developer Mode → right-
  click a channel → Copy ID) into `PARSEH_COORD_DISCORD_CHANNELS`,
  comma-separated.

Nostr environment variables (same env-over-file precedence as GitHub):

- `PARSEH_COORD_NOSTR_NSEC` — the Nostr secret key, either `nsec1…` bech32
  or 64-char hex. **Optional.** If absent (and not in the creds file), the
  connector **generates a fresh keypair on first use**, prints the `nsec`
  to **stderr exactly once** with a loud "SAVE THIS — it is your only
  identity, it is not stored for you" warning, and writes **only** the
  `npub` (public) to `~/.parseh/nostr-identity.txt`. The secret is **never**
  persisted anywhere and is **never logged again** after that one print. To
  reuse the identity you MUST save the printed nsec yourself and set this
  variable (or the creds-file field). The secret is never read from the
  repository, and `~/.parseh/` is gitignored at the repo root.
- `PARSEH_COORD_NOSTR_RELAYS` — comma-separated relay URLs, overrides the
  defaults. Defaults (stable, widely-used **public, unmoderated** relays):
  `wss://relay.damus.io`, `wss://nos.lol`, `wss://relay.nostr.band`.
- `PARSEH_COORD_NOSTR_HASHTAG` — extra hashtag to poll, without the `#`.
  Default `parseh` (i.e. `#parseh`).

`~/.parseh/coord-creds.toml` format:

```toml
github_token = "ghp_xxxxxxxxxxxxxxxxxxxx"
github_repo  = "hiderun-tui/parseh"   # optional; overrides the default

# Codeberg (Forgejo). Both required for any Codeberg operation; codeberg_repo
# has NO default — the mirror must be created/migrated first.
codeberg_token = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
codeberg_repo  = "hiderun-tui/parseh"

# Discord. Both required for any Discord operation; discord_channels has NO
# default. discord_token is a BOT token (sent as `Authorization: Bot …`).
discord_token    = "PUT-YOUR-DISCORD-BOT-TOKEN-HERE"
discord_channels = "000000000000000000,111111111111111111"

# Nostr (all optional). If nostr_nsec is omitted, a fresh key is generated
# on first use and its nsec is printed ONCE to stderr — save it yourself.
nostr_nsec    = "nsec1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
nostr_relays  = "wss://relay.damus.io,wss://nos.lol,wss://relay.nostr.band"
nostr_hashtag = "parseh"
```

## Local store

SQLite at `~/.parseh/coord.db` (override with `--db <path>`). Schema:

- `event(id, platform, kind, thread_ref, author, body, url, created_at,
  ingested_at, answered)` — ingested activity. Append-only in spirit; the
  only mutation is flipping `answered` to 1. Deduped on
  `(platform, thread_ref, author, created_at)`. Indexed on
  `(answered, created_at)` and `(platform, thread_ref)`.
- `outbox(id, platform, thread_ref, body, drafted_at, sent_at, status,
  error, event_id)` — operator drafts. Status state machine:
  `draft → approved → sent` (or `→ failed`).

## The loop: ingest → inbox → draft → approve → send

```bash
# 1. Pull new activity from every connector into the local store.
parseh-coord ingest

# 2. Triage. Newest-first, grouped by platform, id + url + first 200 chars.
parseh-coord inbox
parseh-coord inbox --platform github --limit 20

# 3. Read one in full.
parseh-coord show 42

# 4. Draft a reply (Claude Code typically fills the body in-session).
parseh-coord draft 42 --body "Thanks for filing — here's the honest state…"
#   or pipe it:
echo "..." | parseh-coord draft 42 --stdin

# 5. Review the outbox.
parseh-coord outbox

# 6. Explicitly approve (REQUIRED — send refuses anything not approved).
parseh-coord approve 7

# 7. Send. Posts via the connector; on success marks the linked event
#    answered; on failure stores the error and sets status=failed.
parseh-coord send 7
```

There is no command that combines approve+send, and no flag that skips
approval. That is intentional.

## broadcast-issues

Bulk-create "help wanted / contribute" issues from a TOML (or JSON) file via
the GitHub connector — one API call per issue, created URLs printed. It
still requires the operator to run the command and to have a write-scoped
token. Never silent, never automatic.

```bash
# Preview without any API calls:
parseh-coord broadcast-issues --file contribute-issues.toml --dry-run

# Actually create them (needs PARSEH_COORD_GITHUB_TOKEN with write scope):
parseh-coord broadcast-issues --file contribute-issues.toml

# Same file, posted to the Codeberg (Forgejo) mirror instead. The smallest
# honest seam is one --platform flag on the SAME subcommand (no parallel
# command). Needs PARSEH_COORD_CODEBERG_TOKEN + PARSEH_COORD_CODEBERG_REPO.
# NOTE: issues are created on Codeberg WITHOUT labels (Forgejo's API needs
# integer label IDs, not names — see Limitations).
parseh-coord broadcast-issues --file contribute-issues.toml --platform codeberg
parseh-coord broadcast-issues --file contribute-issues.toml --platform codeberg --dry-run
```

A ready-to-use file, `contribute-issues.toml`, ships in this directory with
~8 well-scoped, alpha-honest issues drawn from the real current project
state. The file format:

```toml
[[issues]]
title  = "help-wanted: <concrete task>"
labels = ["help-wanted", "good-first-issue"]
body   = """
Context · why it matters · honest current state · a concrete first step.
End with a pseudonymous-contribution invite (the project is
pseudonymous-only).
"""

[[issues]]
title  = "help-wanted: another task"
labels = ["help-wanted"]
body   = "…"
```

JSON form (use a `.json` extension): `{"issues": [{"title": "...",
"body": "...", "labels": ["..."]}]}`.

All generated issue content obeys the conservative-semantics rule:
v0.1.0-alpha, network not operational yet, no overclaiming, every open
problem stays open in writing.

## Codeberg

The Codeberg connector is **real**. Codeberg runs **Forgejo** (a Gitea
fork); its REST API base is `https://codeberg.org/api/v1`, very close to
the GitHub REST API. It is wired into the same
ingest→inbox→draft→approve→send loop and the same `broadcast-issues`
command as GitHub:

- `ingest` polls **open issues** (`GET …/issues?state=open&type=issues`)
  and, per issue, that issue's **comments**. Each is normalised into the
  local store (`platform = "codeberg"`, `thread_ref` = the issue number so
  a drafted reply lands on the right issue, author = username, url =
  `html_url`, `created_at` parsed to unix seconds with the same civil-days
  algorithm GitHub uses — no `chrono`). With no creds configured, the
  Codeberg poll bails loudly and `ingest` reports + continues (no fake
  data, no panic), exactly like the Matrix stub.
- `send` on a `codeberg` outbox entry posts an **issue comment** via
  `POST …/issues/{index}/comments`. As with every connector, `send`
  refuses anything not explicitly `approve`d first. There is **no
  auto-posting**; `post()` is only ever reached through the
  already-tested approve→send path.
- `broadcast-issues --platform codeberg` creates issues via
  `POST …/issues`.

Forgejo's auth header is `Authorization: token <TOKEN>` (**not** Bearer);
the connector constructs it that way.

## Discord

The Discord connector is **real**. It uses Discord's REST API
(`https://discord.com/api/v10`) with the same `reqwest` blocking +
`serde_json` approach as the Codeberg connector — **no Discord SDK, no
websocket gateway**. It is **REST polling, not the realtime gateway**: each
`ingest` reads recent messages; it does not stream events live. It is wired
into the same ingest→inbox→draft→approve→send loop:

- `ingest` polls each configured channel
  (`GET /channels/{channel_id}/messages?limit=50`). Each message is
  normalised into the local store (`platform = "discord"`,
  `thread_ref = "discord:<channel>:<message>"` which parses back to the
  channel id so a drafted reply lands in the right channel, author =
  username, body = content, url =
  `https://discord.com/channels/@me/{channel}/{message}` — the guild is
  unknown from this endpoint so the `@me` form is used, `created_at` parsed
  to unix seconds with the same civil-days algorithm GitHub/Codeberg use —
  no `chrono`). Messages authored by a **bot** (which includes this bot
  itself — Discord marks every bot account `author.bot: true`) are
  **skipped**. With no creds configured, the Discord poll bails loudly and
  `ingest` reports + continues (no fake data, no panic), exactly like the
  Matrix stub.
- `send` on a `discord` outbox entry posts a **message** via
  `POST /channels/{channel_id}/messages` with `{"content": "…"}`, the
  channel parsed from the entry's `thread_ref`. As with every connector,
  `send` refuses anything not explicitly `approve`d first. There is **no
  auto-posting**; `post()` is only ever reached through the already-tested
  approve→send path.
- Discord is **messaging only** and is deliberately **not** wired into
  `broadcast-issues` (issues are a GitHub/Codeberg concept).

Discord's bot auth header is `Authorization: Bot <TOKEN>` (literally the
word "Bot", a space, then the token — **not** Bearer, **not** `token`); the
connector constructs it that way.

## Nostr

The Nostr connector is **real**. It is wired into the same
ingest→inbox→draft→approve→send loop as GitHub:

- `ingest` polls Nostr for recent kind-1 notes that **mention our npub**
  (replies / `p`-tag) and recent kind-1 notes carrying the configured
  **hashtag** (default `#parseh`). Each is normalised into the local store
  (`platform = "nostr"`, `thread_ref` = the NIP-10 conversation root id in
  hex, author = a short npub, url = an `https://njump.me/…` link).
- `send` on a `nostr` outbox entry publishes a **kind-1 reply** with the
  correct NIP-10 `e`/`p` tags to the parent referenced by the entry's
  `thread_ref`. To publish a **top-level note** instead, draft against a
  thread ref of `new:` (e.g. `new:announce`). As with every connector,
  `send` refuses anything not explicitly `approve`d first.

There is **no auto-posting**. `post()` is only ever reached through the
already-tested approve→send path.

### Long-form (NIP-23 open letter)

Publish a NIP-23 long-form article (kind 30023) from a markdown file —
intended for the project's "open letter" philosophy post. This is an
explicit operator command (the long-form analogue of approve+send: you run
it, you own the publish — never autonomous):

```bash
# Preview without contacting any relay:
parseh-coord nostr-longform --file open-letter.md --title "Why PARSEH" --dry-run

# Actually publish (best-effort across public relays):
parseh-coord nostr-longform --file open-letter.md --title "Why PARSEH"
```

The `--title` also derives a stable NIP-23 `d` identifier, so re-publishing
the same title **updates** the article (NIP-23 articles are addressable /
replaceable). Output is an `https://njump.me/…` URL.

### Identity safety

If no key is configured the connector generates a fresh keypair on first
use and prints the **nsec once to stderr** with a loud SAVE-THIS warning;
only the **npub** is written to `~/.parseh/nostr-identity.txt`. The secret
is never persisted and never logged again — **save the printed nsec
yourself** or the identity is unrecoverable. See "Credentials" above.

## Commands

| Command | What it does |
|---|---|
| `ingest` | Poll all connectors, upsert (dedupe), print summary counts. |
| `inbox [--platform X] [--limit N]` | Unanswered events, newest-first, grouped. |
| `show <id>` | Full event. |
| `draft <id> --body <text>` / `--stdin` | Store a draft linked to the event's thread. |
| `outbox` | List outbox entries with status (+ last error if failed). |
| `approve <outbox_id>` | `draft → approved`. Fails if not a draft. |
| `send <outbox_id>` | Sends **only if approved**; marks event answered on success. |
| `broadcast-issues --file <path> [--platform github\|codeberg] [--dry-run]` | Create issues from a TOML/JSON file. `--platform` defaults to `github`; `codeberg` posts the same file to the Forgejo mirror (without labels). |
| `nostr-longform --file <path> --title <t> [--dry-run]` | Publish a NIP-23 long-form article (kind 30023) to Nostr. Best-effort across public relays. |

Global: `--db <path>` overrides the SQLite location.

## Limitations (stated, not hidden)

- Discussion comments are ingested (read), but `post()` to a discussion
  thread is not implemented — it returns a clear error. Reply to issue
  threads or open an issue instead.
- GitHub timestamps are parsed without a date library (civil-days
  algorithm); good to the second, best-effort, returns 0 on malformed
  input (GitHub already returns items pre-ordered, so sort order holds).
- **Codeberg is real but its network calls are untested offline** (no
  network in CI — same posture as the GitHub connector). What *is* tested
  offline: issue/comment JSON → `IngestEvent` normalisation, thread_ref
  determinism, env-over-file credential precedence, missing-token /
  missing-repo friendly errors, and the Forgejo `Authorization: token`
  header construction. Delivery depends on **Codeberg availability** — if
  the API is unreachable or returns an error, the command fails with a
  clear error (status set to `failed`), never a fake success.
- **Codeberg `broadcast-issues` does not apply labels.** Forgejo's
  issue-creation API expects `labels` as an array of integer label IDs,
  not names; resolving names → IDs would need an extra round-trip per repo
  and label set, which is out of scope. Issues are created on Codeberg
  **without labels** (the operator is told so on stderr). The `labels`
  field in `contribute-issues.toml` still applies normally on GitHub.
- The Codeberg connector requires `PARSEH_COORD_CODEBERG_REPO` explicitly
  (**no default**) because the project's Codeberg mirror does not exist
  yet; a missing repo or a 404 on the configured repo yields a clear,
  actionable error rather than a panic or a guessed location.
- **Discord is real but uses REST polling, not the realtime gateway.**
  `ingest` reads recent messages on demand; it does not maintain a live
  websocket. A message that scrolled past the last 50 in a channel between
  two `ingest` runs will not be picked up. This is a deliberate scope
  choice (no gateway, no Discord SDK) consistent with the other
  REST-polling connectors.
- **Discord network calls are untested offline** (no network in CI — same
  posture as the GitHub/Codeberg connectors). What *is* tested offline:
  message JSON → `IngestEvent` normalisation, bot/self-message skipping,
  `thread_ref` determinism + parse-back to the channel id, env-over-file
  credential precedence, missing-token / missing-channels friendly errors,
  and the `Authorization: Bot ` header construction. Delivery depends on
  **Discord availability** — if the API is unreachable or returns an error,
  the command fails with a clear error (status set to `failed`), never a
  fake success.
- The Discord connector requires `PARSEH_COORD_DISCORD_CHANNELS` explicitly
  (**no default**) because the project has no fixed Discord channel; a
  missing value yields a clear, friendly error rather than a panic or a
  guessed location. The "Message Content Intent" must be enabled on the bot
  or message bodies arrive empty (a Discord platform requirement, not a
  parseh-coord limitation — stated here so operators are not surprised).
- **Nostr posting is real but best-effort.** Events are published to a
  small set of **public, unmoderated** third-party relays. Delivery is
  **not guaranteed, not anonymous, and not "uncensorable"** — a relay can
  drop, delay, or refuse an event, and there is no read receipt. If no
  relay can be reached the command fails with a clear error (status set to
  `failed`); it never reports a fake success.
- Nostr `poll()` returns recent events from the relays that answered the
  REQ within the timeout; it is a best-effort snapshot, not a guaranteed-
  complete history. A note that no configured relay carries will not be
  seen. Replying needs the parent note to be fetchable from a configured
  relay (to build correct NIP-10 tags); if it cannot be fetched, the send
  fails with a clear error rather than posting a malformed reply.
- The Nostr connector wraps the async `nostr-sdk` in a private blocking
  bridge so the synchronous `Connector` trait shape is unchanged; no async
  leaks past `nostr.rs`.
- Matrix is still a stub. It is wired into the trait so adding it is
  additive; it returns a clear error today.
- The GitHub, Codeberg, Discord, and Nostr **network** calls are
  intentionally not covered by tests (no network in CI). What *is* tested
  offline: the store state machine, the approve-before-send guard, the
  Matrix stub error, the Nostr event→`IngestEvent` normalisation / NIP-10
  reply-tag construction / keypair generation / relay-list parsing /
  credential precedence, the Codeberg issue+comment JSON→`IngestEvent`
  normalisation / thread_ref determinism / env-over-file credential
  precedence / missing-token / missing-repo friendly errors / Forgejo
  `token` auth header, and the Discord message JSON→`IngestEvent`
  normalisation / bot-self-message skipping / thread_ref determinism +
  channel parse-back / env-over-file credential precedence / missing-token
  / missing-channels friendly errors / `Bot ` auth header.
