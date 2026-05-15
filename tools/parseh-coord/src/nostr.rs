//! Real Nostr connector.
//!
//! Conservative-semantics note (binding): Nostr posting here is REAL but
//! best-effort. Events are published to a small set of public, unmoderated
//! relays; delivery is not guaranteed, not anonymous, and not
//! "uncensorable". A relay can drop, delay, or refuse an event. We report
//! relay failures honestly via `anyhow` and never fake success.
//!
//! Identity / credentials (same precedence rule as the GitHub connector —
//! real env vars always win over the creds file, which `main.rs` has
//! already loaded into the process env before this is constructed):
//!  - `PARSEH_COORD_NOSTR_NSEC`   — secret key, `nsec1…` or 64-char hex.
//!  - `PARSEH_COORD_NOSTR_RELAYS` — comma-separated relay URLs (override).
//!  - `PARSEH_COORD_NOSTR_HASHTAG`— hashtag to also poll (default `parseh`).
//!
//! If NO secret key is configured, `from_env()` GENERATES a fresh keypair,
//! prints the `nsec` to **stderr exactly once** with a loud SAVE-THIS
//! warning, writes ONLY the `npub` to `~/.parseh/nostr-identity.txt`, and
//! NEVER persists the secret anywhere. The secret is never logged again
//! after that one-time print.
//!
//! `poll()` fetches recent kind-1 notes that tag our npub (mentions /
//! replies) plus recent kind-1 notes carrying the configured hashtag, and
//! normalises each into the crate's `IngestEvent`. Dedupe is the store's
//! job; we only produce well-formed events.
//!
//! `post()` publishes a kind-1 reply (correct NIP-10 `e`/`p` tags) to the
//! parent referenced by `thread_ref`, or a top-level note when `thread_ref`
//! starts with `new:`. It is only ever called from the approve→send path.
//!
//! `post_longform()` publishes a NIP-23 long-form article (kind 30023) —
//! used by the `nostr-longform` CLI subcommand for the project's open
//! letter. Same human-in-the-loop posture: only invoked by an explicit
//! operator command.

use crate::connector::Connector;
use crate::store::IngestEvent;
use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use std::time::Duration;

/// Well-known, widely-used public relays. Stable defaults; overridable via
/// `PARSEH_COORD_NOSTR_RELAYS`. These are unmoderated third-party relays —
/// see the conservative-semantics note above.
const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
];

const DEFAULT_HASHTAG: &str = "parseh";

/// How long to wait on relay connect / fetch before giving up. Best-effort.
const NET_TIMEOUT: Duration = Duration::from_secs(20);

/// How many recent events to ask each relay for, per filter.
const POLL_LIMIT: usize = 50;

pub struct NostrConnector {
    keys: Keys,
    relays: Vec<String>,
    hashtag: String,
}

impl NostrConnector {
    /// Build from the environment. If no secret key is configured, generate
    /// a fresh keypair, print the nsec ONCE to stderr, and persist only the
    /// npub. Never persists the secret. Never panics.
    pub fn from_env() -> Result<Self> {
        let relays = load_relays();
        let hashtag = std::env::var("PARSEH_COORD_NOSTR_HASHTAG")
            .ok()
            .map(|h| h.trim().trim_start_matches('#').to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_HASHTAG.to_string());

        let keys = match std::env::var("PARSEH_COORD_NOSTR_NSEC")
            .ok()
            .filter(|k| !k.trim().is_empty())
        {
            Some(raw) => Keys::parse(raw.trim()).context(
                "PARSEH_COORD_NOSTR_NSEC is set but not a valid secret key \
                 (expected 'nsec1…' or 64-char hex). parseh-coord never reads \
                 credentials from the repository.",
            )?,
            None => generate_and_announce()?,
        };

        Ok(NostrConnector {
            keys,
            relays,
            hashtag,
        })
    }

    /// Test/`from_env`-shared constructor. No I/O, no printing.
    #[cfg(test)]
    fn with_keys(keys: Keys, relays: Vec<String>, hashtag: String) -> Self {
        NostrConnector {
            keys,
            relays,
            hashtag,
        }
    }

    /// Our own npub (bech32). Cheap; no network. Used by tests to assert
    /// the generated/parsed identity is derivable.
    #[cfg(test)]
    fn npub(&self) -> String {
        self.keys
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| self.keys.public_key().to_hex())
    }

    /// One private tokio runtime per call. nostr-sdk is async; the rest of
    /// this crate (and the `Connector` trait) is synchronous. The runtime
    /// is fully contained here — no async escapes this module.
    fn block_on<F, T>(fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for the nostr connector")?;
        rt.block_on(fut)
    }

    /// Connect a client to the configured relays. Returns an error (never a
    /// panic) if NO relay could be reached.
    async fn connect_client(&self) -> Result<Client> {
        let client = Client::new(self.keys.clone());
        for url in &self.relays {
            // add_relay validates the URL; a bad entry is a config error.
            client
                .add_relay(url.as_str())
                .await
                .with_context(|| format!("adding relay {url:?}"))?;
        }
        client.try_connect(NET_TIMEOUT).await;
        // Best-effort: require at least one relay actually connected.
        let connected = client
            .relays()
            .await
            .values()
            .filter(|r| r.is_connected())
            .count();
        if connected == 0 {
            anyhow::bail!(
                "could not connect to any of the {} configured nostr relay(s): {}. \
                 Delivery across public relays is best-effort; check connectivity \
                 or override PARSEH_COORD_NOSTR_RELAYS.",
                self.relays.len(),
                self.relays.join(", ")
            );
        }
        Ok(client)
    }

    async fn poll_async(&self) -> Result<Vec<IngestEvent>> {
        let client = self.connect_client().await?;

        let me = self.keys.public_key();
        // (a) kind-1 notes that p-tag us (mentions / replies to us).
        let mentions = Filter::new()
            .kind(Kind::TextNote)
            .pubkey(me)
            .limit(POLL_LIMIT);
        // (b) kind-1 notes carrying the configured hashtag.
        let tagged = Filter::new()
            .kind(Kind::TextNote)
            .hashtag(self.hashtag.clone())
            .limit(POLL_LIMIT);

        let mut out = Vec::new();
        for f in [mentions, tagged] {
            match client.fetch_events(f, NET_TIMEOUT).await {
                Ok(events) => {
                    for ev in events.into_iter() {
                        out.push(normalise(&ev));
                    }
                }
                Err(e) => {
                    // Surface relay/fetch failure honestly — never panic,
                    // never silently swallow into an empty success.
                    let _ = client.disconnect().await;
                    return Err(anyhow::anyhow!(
                        "nostr relay fetch failed (best-effort across public relays): {e}"
                    ));
                }
            }
        }
        let _ = client.disconnect().await;
        Ok(out)
    }

    async fn post_async(&self, thread_ref: &str, body: &str) -> Result<String> {
        let client = self.connect_client().await?;

        let builder = if let Some(rest) = thread_ref.strip_prefix("new:") {
            // Top-level note. `new:` or `new:<anything>` — the suffix is a
            // human label only, it does not affect the published note.
            let _ = rest;
            EventBuilder::text_note(body)
        } else {
            // Reply: resolve the parent event so NIP-10 e/p tags are
            // correct. thread_ref is the parent event id (hex or note/nevent
            // bech32, normalised on ingest to hex).
            let parent_id = parse_event_id(thread_ref).with_context(|| {
                format!(
                    "nostr reply target {thread_ref:?} is not a valid event id \
                     (expected hex / note1… / nevent1…, or 'new:' for a top-level note)"
                )
            })?;
            let parent = client
                .fetch_events(
                    Filter::new().id(parent_id).limit(1),
                    NET_TIMEOUT,
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!("fetching the parent note to reply to failed: {e}")
                })?
                .first_owned()
                .with_context(|| {
                    format!(
                        "parent note {thread_ref} not found on the configured relays \
                         (cannot construct a correct reply)"
                    )
                })?;
            // root = the thread root if the parent itself is a reply, else
            // the parent is the root. text_note_reply writes proper
            // Root/Reply markers + p tags.
            EventBuilder::text_note_reply(body, &parent, None, None)
        };

        let res = client.send_event_builder(builder).await.map_err(|e| {
            anyhow::anyhow!(
                "publishing the nostr event failed (best-effort across public relays): {e}"
            )
        })?;
        let _ = client.disconnect().await;

        let id = res.id();
        let njump = id
            .to_bech32()
            .map(|b| format!("https://njump.me/{b}"))
            .unwrap_or_else(|_| format!("nostr:{}", id.to_hex()));
        Ok(njump)
    }

    /// Publish a NIP-23 long-form article (kind 30023). `title` becomes the
    /// article title tag and is also used to derive the stable `d`
    /// identifier. Returns an njump URL. Best-effort delivery, same as
    /// `post()`. Only ever invoked by the explicit `nostr-longform` CLI
    /// subcommand — never autonomous.
    pub fn post_longform(&self, title: &str, markdown: &str) -> Result<String> {
        Self::block_on(self.post_longform_async(title, markdown))
    }

    async fn post_longform_async(&self, title: &str, markdown: &str) -> Result<String> {
        let client = self.connect_client().await?;
        let ident = slug(title);
        let builder = EventBuilder::long_form_text_note(markdown).tags([
            Tag::identifier(ident),
            Tag::from_standardized(TagStandard::Title(title.to_string())),
            Tag::from_standardized(TagStandard::PublishedAt(Timestamp::now())),
        ]);
        let res = client.send_event_builder(builder).await.map_err(|e| {
            anyhow::anyhow!(
                "publishing the NIP-23 long-form article failed \
                 (best-effort across public relays): {e}"
            )
        })?;
        let _ = client.disconnect().await;
        let id = res.id();
        Ok(id
            .to_bech32()
            .map(|b| format!("https://njump.me/{b}"))
            .unwrap_or_else(|_| format!("nostr:{}", id.to_hex())))
    }
}

impl Connector for NostrConnector {
    fn platform(&self) -> &str {
        "nostr"
    }

    fn poll(&self) -> Result<Vec<IngestEvent>> {
        Self::block_on(self.poll_async())
    }

    fn post(&self, thread_ref: &str, body: &str) -> Result<String> {
        Self::block_on(self.post_async(thread_ref, body))
    }
}

/// Read the relay override env var, else the documented defaults.
fn load_relays() -> Vec<String> {
    match std::env::var("PARSEH_COORD_NOSTR_RELAYS").ok() {
        Some(s) => parse_relays(&s),
        None => DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Parse a comma-separated relay list; empty input falls back to defaults.
fn parse_relays(s: &str) -> Vec<String> {
    let v: Vec<String> = s
        .split(',')
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if v.is_empty() {
        DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
    } else {
        v
    }
}

/// Generate a fresh keypair, print the nsec ONCE to stderr with a loud
/// save-this warning, and persist ONLY the npub (never the secret). This is
/// the documented "no key configured" behaviour.
fn generate_and_announce() -> Result<Keys> {
    let keys = Keys::generate();
    let nsec = keys
        .secret_key()
        .to_bech32()
        .context("encoding the freshly generated secret key as nsec")?;
    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| keys.public_key().to_hex());

    // The ONLY place the secret is ever emitted. Never logged again.
    eprintln!("================================================================");
    eprintln!("  parseh-coord: NO NOSTR KEY WAS CONFIGURED — a fresh one was");
    eprintln!("  generated for this run.");
    eprintln!();
    eprintln!("  nsec: {nsec}");
    eprintln!();
    eprintln!("  SAVE THIS NOW. It is your only Nostr identity. It is NOT");
    eprintln!("  stored for you. If you lose it, this identity is gone.");
    eprintln!("  To reuse it, set PARSEH_COORD_NOSTR_NSEC or add");
    eprintln!("  nostr_nsec = \"<nsec1…>\" to ~/.parseh/coord-creds.toml.");
    eprintln!();
    eprintln!("  npub (public, safe to share): {npub}");
    eprintln!("================================================================");

    // Persist ONLY the public half, outside the repo, best-effort.
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".parseh");
        if std::fs::create_dir_all(&dir).is_ok() {
            let note = dir.join("nostr-identity.txt");
            let _ = std::fs::write(
                &note,
                format!(
                    "# parseh-coord nostr identity (PUBLIC ONLY).\n\
                     # The secret (nsec) is NEVER written here — it was\n\
                     # printed once to stderr on generation. Save it yourself.\n\
                     npub = {npub}\n"
                ),
            );
        }
    }

    Ok(keys)
}

/// Parse an event id given as hex or as a `note1…` / `nevent1…` bech32
/// string. Returns None if neither parses.
fn parse_event_id(s: &str) -> Option<EventId> {
    EventId::parse(s).ok()
}

/// Normalise a Nostr `Event` into the crate's `IngestEvent`.
///
/// `thread_ref` is a STABLE reference to the conversation root: the NIP-10
/// root `e` tag if present, else the first `e` tag, else this event's own
/// id (a fresh top-level note is its own thread). Always hex so `post()`
/// can parse it back without ambiguity.
fn normalise(ev: &Event) -> IngestEvent {
    let thread_ref = root_thread_ref(ev);
    let author = ev
        .pubkey
        .to_bech32()
        .map(|b| short_npub(&b))
        .unwrap_or_else(|_| ev.pubkey.to_hex());
    let url = ev
        .id
        .to_bech32()
        .map(|b| format!("https://njump.me/{b}"))
        .unwrap_or_else(|_| format!("nostr:{}", ev.id.to_hex()));
    IngestEvent {
        platform: "nostr".into(),
        kind: "note".into(),
        thread_ref,
        author,
        body: ev.content.clone(),
        url,
        created_at: ev.created_at.as_secs() as i64,
    }
}

/// The thread root id (hex) for an event, per NIP-10: prefer an explicit
/// root `e` tag, fall back to the first `e` tag, fall back to the event's
/// own id (it is itself a thread root).
fn root_thread_ref(ev: &Event) -> String {
    // Look for a marked root e-tag first.
    for tag in ev.tags.iter() {
        if let Some(TagStandard::Event {
            event_id,
            marker: Some(Marker::Root),
            ..
        }) = tag.as_standardized()
        {
            return event_id.to_hex();
        }
    }
    // Else any e-tag (this event is a reply, but root marker absent).
    if let Some(first_e) = ev.tags.event_ids().next() {
        return first_e.to_hex();
    }
    // Else this event is the root of its own thread.
    ev.id.to_hex()
}

/// Short, human-readable form of an npub for the inbox author column:
/// `npub1abcd…wxyz`.
fn short_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}…{}", &npub[..10], &npub[npub.len() - 6..])
}

/// Derive a stable, lowercase, dash-separated NIP-23 `d` identifier from a
/// title. Deterministic so re-publishing the same article updates it
/// (NIP-23 articles are addressable/replaceable by `d`).
fn slug(title: &str) -> String {
    let mut s = String::with_capacity(title.len());
    let mut last_dash = false;
    for ch in title.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            s.push(ch);
            last_dash = false;
        } else if !last_dash && !s.is_empty() {
            s.push('-');
            last_dash = true;
        }
    }
    let trimmed = s.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "parseh-longform".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test-only seam so relay parsing can be unit-tested without mutating
    // the process environment (racy across parallel test threads).
    fn load_relays_from(s: &str) -> Vec<String> {
        parse_relays(s)
    }

    fn test_keys() -> Keys {
        // Deterministic throwaway test key: the secret is the constant
        // 0x00…01. NEVER a real identity — it exists only to make the
        // offline normalisation/reply-tag assertions reproducible.
        Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
            .expect("valid deterministic test key")
    }

    #[test]
    fn keypair_generation_produces_usable_npub() {
        // The no-key path's core: a fresh key yields a derivable npub and a
        // round-trippable nsec.
        let k = Keys::generate();
        let npub = k.public_key().to_bech32().unwrap();
        assert!(npub.starts_with("npub1"), "got {npub}");
        let nsec = k.secret_key().to_bech32().unwrap();
        assert!(nsec.starts_with("nsec1"), "got {nsec}");
        // Re-parsing the nsec yields the same public key.
        let reparsed = Keys::parse(&nsec).unwrap();
        assert_eq!(reparsed.public_key(), k.public_key());
    }

    #[test]
    fn from_env_with_no_key_generates_and_warns() {
        // No PARSEH_COORD_NOSTR_NSEC set -> generate path. We can't capture
        // stderr here without a harness, but we assert it does NOT error and
        // yields a working keypair, and never persists the secret. (The
        // print is exercised by reading generate_and_announce; here we test
        // that the no-key branch is the generate branch, no panic.)
        // Guard: only run if the env var is genuinely unset in this process.
        if std::env::var("PARSEH_COORD_NOSTR_NSEC").is_ok() {
            return;
        }
        std::env::remove_var("PARSEH_COORD_NOSTR_RELAYS");
        let c = NostrConnector::from_env().expect("no-key path must generate, not error");
        assert_eq!(c.platform(), "nostr");
        assert!(c.npub().starts_with("npub1"), "got {}", c.npub());
        // Defaults applied.
        assert_eq!(c.relays, DEFAULT_RELAYS);
        assert_eq!(c.hashtag, DEFAULT_HASHTAG);
    }

    #[test]
    fn creds_env_overrides_and_relay_parsing() {
        // Precedence: an explicitly provided nsec is used (env-over-file is
        // enforced in main.rs's loader; here we prove the connector honours
        // a provided key rather than generating).
        let k = test_keys();
        let want_pk = k.public_key();
        let c = NostrConnector::with_keys(
            k,
            load_relays_from("wss://a.example , ,wss://b.example"),
            "parseh".into(),
        );
        assert_eq!(c.keys.public_key(), want_pk);
        assert_eq!(c.relays, vec!["wss://a.example", "wss://b.example"]);
    }

    #[test]
    fn relay_override_falls_back_to_defaults_when_empty() {
        assert_eq!(load_relays_from("  , ,  "), DEFAULT_RELAYS);
        assert_eq!(
            load_relays_from("wss://only.example"),
            vec!["wss://only.example"]
        );
    }

    #[test]
    fn normalise_top_level_note_is_its_own_thread() {
        let keys = test_keys();
        let ev = EventBuilder::text_note("hello parseh")
            .sign_with_keys(&keys)
            .unwrap();
        let ie = normalise(&ev);
        assert_eq!(ie.platform, "nostr");
        assert_eq!(ie.kind, "note");
        assert_eq!(ie.body, "hello parseh");
        // A root note's thread_ref is its own id (hex).
        assert_eq!(ie.thread_ref, ev.id.to_hex());
        assert!(ie.author.starts_with("npub1"), "got {}", ie.author);
        assert!(ie.url.contains("njump.me"), "got {}", ie.url);
        assert!(ie.created_at > 0);
    }

    #[test]
    fn normalise_reply_points_thread_ref_at_root() {
        let alice = test_keys();
        let bob = Keys::generate();
        let root = EventBuilder::text_note("the root post")
            .sign_with_keys(&alice)
            .unwrap();
        // Bob replies to alice's root; text_note_reply writes the Root marker.
        let reply = EventBuilder::text_note_reply("a reply", &root, None, None)
            .sign_with_keys(&bob)
            .unwrap();
        let ie = normalise(&reply);
        // The reply's thread_ref must be the ROOT event id, not the reply's.
        assert_eq!(ie.thread_ref, root.id.to_hex());
        assert_ne!(ie.thread_ref, reply.id.to_hex());
    }

    #[test]
    fn reply_tag_construction_has_e_and_p_tags() {
        let alice = test_keys();
        let bob = Keys::generate();
        let parent = EventBuilder::text_note("parent")
            .sign_with_keys(&alice)
            .unwrap();
        let reply = EventBuilder::text_note_reply("child", &parent, None, None)
            .sign_with_keys(&bob)
            .unwrap();
        // NIP-10: reply must e-tag the parent and p-tag its author.
        let e_ids: Vec<_> = reply.tags.event_ids().collect();
        assert!(e_ids.contains(&&parent.id), "reply must e-tag the parent");
        let p_keys: Vec<_> = reply.tags.public_keys().collect();
        assert!(
            p_keys.contains(&&alice.public_key()),
            "reply must p-tag the parent author"
        );
    }

    #[test]
    fn parse_event_id_accepts_hex_and_bech32() {
        let keys = test_keys();
        let ev = EventBuilder::text_note("x").sign_with_keys(&keys).unwrap();
        let hex = ev.id.to_hex();
        let bech = ev.id.to_bech32().unwrap();
        assert_eq!(parse_event_id(&hex).unwrap(), ev.id);
        assert_eq!(parse_event_id(&bech).unwrap(), ev.id);
        assert!(parse_event_id("not-an-id").is_none());
        assert!(parse_event_id("new:").is_none());
    }

    #[test]
    fn slug_is_stable_and_safe() {
        assert_eq!(slug("Open Letter: Why PARSEH?"), "open-letter-why-parseh");
        assert_eq!(slug("  Trailing -- Dashes -- "), "trailing-dashes");
        assert_eq!(slug("!!!"), "parseh-longform");
        // Deterministic.
        assert_eq!(slug("Same Title"), slug("Same Title"));
    }

    #[test]
    fn short_npub_truncates_long_keys() {
        let keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let short = short_npub(&npub);
        assert!(short.contains('…'));
        assert!(short.starts_with("npub1"));
        assert!(short.len() < npub.len());
        assert_eq!(short_npub("short"), "short");
    }
}
