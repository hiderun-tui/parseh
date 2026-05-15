//! Real Discord connector.
//!
//! Discord's REST API base is `https://discord.com/api/v10`. This connector
//! uses `reqwest` blocking + `serde_json` exactly like `codeberg.rs` — no
//! heavyweight Discord SDK, no gateway/websocket, no async leaking past this
//! file. It is REST polling, not the realtime gateway (stated honestly in
//! the README "Limitations").
//!
//! The one meaningful Discord difference vs the other connectors: the auth
//! header value for a bot token is literally `Bot <TOKEN>` (the word "Bot",
//! a space, then the token) — NOT `Bearer`, NOT `token`.
//!
//! Credentials are read from the environment ONLY (or, indirectly, from
//! `~/.parseh/coord-creds.toml` which `main.rs` loads into the process env
//! before constructing this). Nothing is ever read from the repo.
//!
//!  - `PARSEH_COORD_DISCORD_TOKEN`    — a Discord bot token. REQUIRED for
//!    any operation. Never logged.
//!  - `PARSEH_COORD_DISCORD_CHANNELS` — comma-separated channel IDs to
//!    poll. REQUIRED (no default: the project has no fixed Discord channel,
//!    so guessing one would be dishonest).
//!
//! `poll()` reads recent messages from each configured channel (REST).
//! `post()` posts a message to the channel encoded in the `thread_ref`.
//! Discord is messaging only — it is deliberately NOT wired into
//! `broadcast-issues` (issues are a GitHub/Codeberg concept).
//!
//! If the token or channels are absent every method fails with a friendly
//! error that names the exact env var. We never panic. Bot/self messages
//! are skipped so the operator never sees the bot talking to itself.

use crate::connector::Connector;
use crate::store::IngestEvent;
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

const API_BASE: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "parseh-coord/0.1.0-alpha (+https://github.com/hiderun-tui/parseh)";

pub struct DiscordConnector {
    token: Option<String>,
    /// Channel IDs to poll. Only populated when the env/creds value was
    /// present and non-empty; otherwise empty so the missing-channels path
    /// produces a friendly error instead of a panic.
    channels: Vec<String>,
    client: reqwest::blocking::Client,
}

impl DiscordConnector {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("PARSEH_COORD_DISCORD_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        // No default channel: unlike GitHub there is no known fixed
        // location. A missing/blank value yields an empty Vec and a
        // friendly error at call time (never a panic).
        let channels = std::env::var("PARSEH_COORD_DISCORD_CHANNELS")
            .ok()
            .map(|raw| parse_channels(&raw))
            .unwrap_or_default();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;
        Ok(DiscordConnector {
            token,
            channels,
            client,
        })
    }

    fn token(&self) -> Result<&str> {
        self.token.as_deref().context(
            "Discord token missing: set the PARSEH_COORD_DISCORD_TOKEN environment variable \
             (a Discord bot token), or add it to ~/.parseh/coord-creds.toml as \
             `discord_token`. parseh-coord never reads credentials from the repository.",
        )
    }

    fn channels(&self) -> Result<&[String]> {
        if self.channels.is_empty() {
            anyhow::bail!(
                "Discord channels missing: set the PARSEH_COORD_DISCORD_CHANNELS \
                 environment variable to a comma-separated list of channel IDs (or add \
                 `discord_channels` to ~/.parseh/coord-creds.toml). There is no default \
                 — the project has no fixed Discord channel."
            );
        }
        Ok(&self.channels)
    }

    /// Discord bot auth: `Authorization: Bot <TOKEN>` (NOT Bearer, NOT
    /// `token`). The single most important Discord-specific detail.
    fn auth_header(token: &str) -> String {
        format!("Bot {token}")
    }

    fn rest_get(&self, url: &str) -> Result<Value> {
        let token = self.token()?;
        let resp = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json")
            .header("Authorization", Self::auth_header(token))
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Discord GET {url} -> HTTP {status}: {}", truncate(&text, 300));
        }
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
    }

    fn poll_channel(&self, channel_id: &str) -> Result<Vec<IngestEvent>> {
        // limit=50 is the same modest page size the other connectors use.
        // The local store dedupes, so we don't need an `&after=` high-water
        // mark to stay correct — we just emit well-formed events.
        let url = format!("{API_BASE}/channels/{channel_id}/messages?limit=50");
        let arr = self.rest_get(&url)?;
        let mut out = Vec::new();
        for msg in arr.as_array().cloned().unwrap_or_default() {
            if let Some(ev) = message_to_event(&msg, channel_id) {
                out.push(ev);
            }
        }
        Ok(out)
    }
}

impl Connector for DiscordConnector {
    fn platform(&self) -> &str {
        "discord"
    }

    fn poll(&self) -> Result<Vec<IngestEvent>> {
        // Fail early + friendly if token or channels are missing.
        self.token()?;
        let channels = self.channels()?.to_vec();
        let mut all = Vec::new();
        for ch in &channels {
            all.extend(self.poll_channel(ch)?);
        }
        Ok(all)
    }

    fn post(&self, thread_ref: &str, body: &str) -> Result<String> {
        let token = self.token()?;
        let channel_id = channel_from_thread_ref(thread_ref)?;
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let resp = self
            .client
            .post(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json")
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "content": body }))
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Discord post message -> HTTP {status}: {}",
                truncate(&text, 400)
            );
        }
        let v: Value = serde_json::from_str(&text).context("parsing posted-message JSON")?;
        let msg_id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
        Ok(message_url(&channel_id, msg_id))
    }
}

/// Parse a comma-separated channel list, trimming whitespace and dropping
/// empty entries.
fn parse_channels(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Stable thread_ref encoding `discord:<channel>:<message>`. It must parse
/// back to the channel id so `post()` lands a reply in the right channel.
fn make_thread_ref(channel_id: &str, message_id: &str) -> String {
    format!("discord:{channel_id}:{message_id}")
}

/// Recover the channel id from a thread_ref produced by `make_thread_ref`.
/// Rejects anything malformed before any network call.
fn channel_from_thread_ref(thread_ref: &str) -> Result<String> {
    let mut parts = thread_ref.split(':');
    match (parts.next(), parts.next()) {
        (Some("discord"), Some(ch)) if !ch.is_empty() => Ok(ch.to_string()),
        _ => anyhow::bail!(
            "discord thread_ref must be 'discord:<channel>:<message>', got {thread_ref:?}"
        ),
    }
}

/// `https://discord.com/channels/@me/{channel}/{message}` — used when the
/// guild is unknown (the messages endpoint does not return the guild id).
fn message_url(channel_id: &str, message_id: &str) -> String {
    format!("https://discord.com/channels/@me/{channel_id}/{message_id}")
}

/// Normalise one Discord message JSON object into an `IngestEvent`.
/// Returns `None` for messages we deliberately skip: anything authored by
/// a bot (which includes this bot itself — bot accounts always have
/// `author.bot == true`).
fn message_to_event(msg: &Value, channel_id: &str) -> Option<IngestEvent> {
    // Skip bot/self messages so the operator never sees the bot talking to
    // itself. Discord marks every bot account with `author.bot: true`.
    if msg
        .pointer("/author/bot")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    let message_id = msg.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let author = msg
        .pointer("/author/username")
        .and_then(|u| u.as_str())
        .unwrap_or("(unknown)")
        .to_string();
    Some(IngestEvent {
        platform: "discord".into(),
        kind: "message".into(),
        thread_ref: make_thread_ref(channel_id, message_id),
        author,
        body: msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        url: message_url(channel_id, message_id),
        created_at: parse_ts(msg.get("timestamp").and_then(|t| t.as_str())),
    })
}

/// Parse an RFC3339 timestamp into unix seconds without pulling chrono.
/// Best-effort: returns 0 on any parse failure. Discord returns
/// `timestamp` as ISO-8601 (`2026-05-15T16:29:00.123000+00:00` or
/// `…Z`); the leading `YYYY-MM-DDTHH:MM:SS` slice is the same shape
/// github.rs/codeberg.rs already parse, so this is the identical
/// civil-days algorithm. Sub-second / offset suffixes are ignored
/// (acceptable: the store only uses this for ordering, and Discord
/// timestamps are UTC).
fn parse_ts(s: Option<&str>) -> i64 {
    let s = match s {
        Some(s) => s,
        None => return 0,
    };
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return 0;
    }
    let g = |a: usize, b: usize| s[a..b].parse::<i64>().unwrap_or(0);
    let (y, mo, d) = (g(0, 4), g(5, 7), g(8, 10));
    let (h, mi, se) = (g(11, 13), g(14, 16), g(17, 19));
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + h * 3600 + mi * 60 + se
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_token_and_channels() -> DiscordConnector {
        DiscordConnector {
            token: Some("fake".into()),
            channels: vec!["111".into(), "222".into()],
            client: reqwest::blocking::Client::new(),
        }
    }

    #[test]
    fn discord_auth_header_is_bot_prefixed_not_bearer() {
        // The single most important Discord difference: literally
        // `Bot <TOKEN>` (the word "Bot", a space, then the token).
        assert_eq!(DiscordConnector::auth_header("abc123"), "Bot abc123");
        assert!(!DiscordConnector::auth_header("abc123").starts_with("Bearer"));
        assert!(!DiscordConnector::auth_header("abc123").starts_with("token "));
    }

    #[test]
    fn parse_ts_matches_github_shape() {
        // Plain `…Z` form.
        let a = parse_ts(Some("2026-05-15T00:00:00Z"));
        let b = parse_ts(Some("2026-05-15T00:00:01Z"));
        let c = parse_ts(Some("2026-05-16T00:00:00Z"));
        assert!(a > 1_700_000_000, "got {a}");
        assert_eq!(b - a, 1);
        assert_eq!(c - a, 86400);
        assert_eq!(parse_ts(None), 0);
        assert_eq!(parse_ts(Some("garbage")), 0);
        // Discord's sub-second + offset form must parse the same instant
        // (sub-second / offset suffix ignored — UTC, ordering only).
        let d = parse_ts(Some("2026-05-15T00:00:00.123000+00:00"));
        assert_eq!(d, a);
    }

    #[test]
    fn parse_channels_trims_and_drops_empties() {
        assert_eq!(parse_channels("111,222"), vec!["111", "222"]);
        assert_eq!(parse_channels("  111 , , 222 ,"), vec!["111", "222"]);
        assert!(parse_channels("").is_empty());
        assert!(parse_channels("  ,  ").is_empty());
    }

    #[test]
    fn thread_ref_round_trips_to_channel_id() {
        let tr = make_thread_ref("9876543210", "1234567890");
        assert_eq!(tr, "discord:9876543210:1234567890");
        // Must parse back to the *channel* id (not the message id) so a
        // reply lands in the right channel.
        assert_eq!(channel_from_thread_ref(&tr).unwrap(), "9876543210");
    }

    #[test]
    fn thread_ref_is_deterministic() {
        // Same channel+message → identical thread_ref every time, so
        // dedupe + reply targeting are stable.
        let a = make_thread_ref("c1", "m1");
        let b = make_thread_ref("c1", "m1");
        assert_eq!(a, b);
    }

    #[test]
    fn post_rejects_malformed_thread_ref_before_network() {
        let c = with_token_and_channels();
        for bad in ["1234", "discord:", "github:1:2", "discord::5"] {
            let e = c.post(bad, "hi").unwrap_err().to_string();
            assert!(
                e.contains("discord thread_ref must be"),
                "thread_ref {bad:?} -> {e}"
            );
        }
    }

    #[test]
    fn from_env_reads_token_and_channels_env_first() {
        // Precedence: a real env var is honoured by from_env() (the
        // env-over-file rule itself is enforced in main.rs's loader, which
        // only sets the var from the creds file when the env var is
        // ABSENT — so if the env var is present, from_env() observes
        // exactly it, never the file). Using unique var-free names here is
        // not possible since from_env() reads fixed names; this test runs
        // single-threaded-safe because it sets then immediately removes.
        std::env::set_var("PARSEH_COORD_DISCORD_TOKEN", "env-token-xyz");
        std::env::set_var("PARSEH_COORD_DISCORD_CHANNELS", " 111 , 222 ");
        let c = DiscordConnector::from_env().expect("from_env");
        assert_eq!(c.token.as_deref(), Some("env-token-xyz"));
        assert_eq!(c.channels, vec!["111".to_string(), "222".to_string()]);
        std::env::remove_var("PARSEH_COORD_DISCORD_TOKEN");
        std::env::remove_var("PARSEH_COORD_DISCORD_CHANNELS");
        // With the vars cleared, from_env() yields the friendly-error
        // (None/empty) shape — never a panic.
        let c2 = DiscordConnector::from_env().expect("from_env no-creds");
        assert!(c2.token.is_none());
        assert!(c2.channels.is_empty());
    }

    #[test]
    fn missing_token_is_friendly_not_panic() {
        let dc = DiscordConnector {
            token: None,
            channels: vec!["111".into()],
            client: reqwest::blocking::Client::new(),
        };
        let pe = dc.poll().unwrap_err().to_string();
        assert!(pe.contains("PARSEH_COORD_DISCORD_TOKEN"), "got: {pe}");
        let se = dc
            .post("discord:111:222", "x")
            .unwrap_err()
            .to_string();
        assert!(se.contains("PARSEH_COORD_DISCORD_TOKEN"), "got: {se}");
    }

    #[test]
    fn missing_channels_is_friendly_not_panic() {
        // Token present, channels absent: poll must name the channels env
        // var, not panic.
        let dc = DiscordConnector {
            token: Some("fake".into()),
            channels: vec![],
            client: reqwest::blocking::Client::new(),
        };
        let pe = dc.poll().unwrap_err().to_string();
        assert!(pe.contains("PARSEH_COORD_DISCORD_CHANNELS"), "got: {pe}");
    }

    // --- JSON → IngestEvent normalisation (offline; no network) ---

    #[test]
    fn message_json_normalises_to_ingest_event() {
        let msg = serde_json::json!({
            "id": "1234567890",
            "content": "is the miner binary working on arm?",
            "timestamp": "2026-05-15T16:29:00.000000+00:00",
            "author": { "username": "contributor1", "bot": false }
        });
        let ev = message_to_event(&msg, "9876543210").expect("non-bot message");
        assert_eq!(ev.platform, "discord");
        assert_eq!(ev.kind, "message");
        assert_eq!(ev.thread_ref, "discord:9876543210:1234567890");
        assert_eq!(ev.author, "contributor1");
        assert_eq!(ev.body, "is the miner binary working on arm?");
        assert_eq!(
            ev.url,
            "https://discord.com/channels/@me/9876543210/1234567890"
        );
        assert!(ev.created_at > 1_700_000_000, "got {}", ev.created_at);
        // thread_ref must recover the channel id for reply targeting.
        assert_eq!(
            channel_from_thread_ref(&ev.thread_ref).unwrap(),
            "9876543210"
        );
    }

    #[test]
    fn bot_authored_message_is_skipped() {
        // Discord marks every bot account (including this bot itself) with
        // author.bot == true. We must never surface those.
        let msg = serde_json::json!({
            "id": "1",
            "content": "auto reply from some bot",
            "timestamp": "2026-05-15T16:29:00.000000+00:00",
            "author": { "username": "parseh-coord-bot", "bot": true }
        });
        assert!(message_to_event(&msg, "c").is_none());
    }

    #[test]
    fn missing_author_bot_flag_defaults_to_human() {
        // Absent `bot` field → treat as a human (do not silently drop real
        // messages). A human message with no bot flag must come through.
        let msg = serde_json::json!({
            "id": "2",
            "content": "hello",
            "timestamp": "2026-05-15T16:29:00Z",
            "author": { "username": "human" }
        });
        let ev = message_to_event(&msg, "c").expect("human message kept");
        assert_eq!(ev.author, "human");
    }
}
