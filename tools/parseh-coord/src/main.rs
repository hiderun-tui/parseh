//! parseh-coord — operator-driven community notification / messaging /
//! issue-broadcast tool for the PARSEH / Hiderun project.
//!
//! HUMAN-IN-THE-LOOP IS A HARD RULE. This is NOT an autonomous daemon.
//! Nothing is ever posted to a human automatically. Every outbound message
//! must be explicitly drafted, explicitly `approve`d, and explicitly `send`.
//! `send` refuses any entry whose status is not exactly `approved`.
//!
//! Scope honesty (v0.1.0-alpha; the PARSEH network is NOT operational yet):
//!  - GitHub connector: REAL.
//!  - Codeberg connector: REAL (Forgejo REST). Network calls untested
//!    offline; delivery depends on Codeberg availability. See `codeberg.rs`.
//!  - Discord connector: REAL (REST polling, not the realtime gateway).
//!    Network calls untested offline; delivery depends on Discord
//!    availability. See `discord.rs`.
//!  - Nostr connector: REAL — posting is best-effort across public,
//!    unmoderated relays (not guaranteed / not anonymous / not
//!    "uncensorable"). See `nostr.rs` and the README.
//!  - Matrix connector: STUBBED (bails with a clear message).

mod codeberg;
mod connector;
mod discord;
mod github;
mod nostr;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use codeberg::CodebergConnector;
use connector::{Connector, MatrixConnector};
use discord::DiscordConnector;
use github::GithubConnector;
use nostr::NostrConnector;
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use store::{OutboxStatus, Store};

#[derive(Parser)]
#[command(
    name = "parseh-coord",
    version,
    about = "PARSEH community notification + messaging + issue-broadcast (human-in-the-loop only).",
    long_about = "Operator tool. Not an autonomous daemon — every send requires an explicit prior approve. \
GitHub + Nostr connectors are real (Nostr posting is best-effort across public relays); Matrix is stubbed. \
PARSEH is v0.1.0-alpha; the network is not operational yet."
)]
struct Cli {
    /// Override the SQLite path (default: ~/.parseh/coord.db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll every connector and upsert new events into the local store.
    Ingest,
    /// List unanswered events, newest-first, grouped by platform.
    Inbox {
        #[arg(long)]
        platform: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Show a single event in full.
    Show { id: i64 },
    /// Write a reply draft into the outbox, linked to an event's thread.
    Draft {
        id: i64,
        /// Reply body.
        #[arg(long, conflicts_with = "stdin")]
        body: Option<String>,
        /// Read the body from stdin instead of --body.
        #[arg(long)]
        stdin: bool,
    },
    /// List outbox entries with their status.
    Outbox,
    /// Flip a draft to approved (required before send).
    Approve { outbox_id: i64 },
    /// Send an APPROVED outbox entry via its connector. Refuses non-approved.
    Send { outbox_id: i64 },
    /// Create "help wanted" issues from a TOML/JSON file. Defaults to
    /// GitHub; pass `--platform codeberg` to create the same issues on the
    /// Codeberg (Forgejo) mirror instead.
    BroadcastIssues {
        #[arg(long)]
        file: PathBuf,
        /// Target platform: `github` (default) or `codeberg`.
        #[arg(long, default_value = "github")]
        platform: String,
        /// Print what would be created without making API calls.
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish a NIP-23 long-form article (kind 30023) to Nostr from a
    /// markdown file — for the project's open letter. Best-effort across
    /// public relays. This is an explicit operator command (never
    /// autonomous), the long-form analogue of approve+send: you run it,
    /// you own the publish.
    NostrLongform {
        /// Markdown file (the article body).
        #[arg(long)]
        file: PathBuf,
        /// Article title (also derives the stable NIP-23 `d` identifier).
        #[arg(long)]
        title: String,
        /// Print what would be published without contacting any relay.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Optional credentials file at ~/.parseh/coord-creds.toml. Loaded into the
/// process environment so connectors can read it via the same env vars.
/// Format documented in the crate README. Never read from the repo.
#[derive(Debug, Deserialize, Default)]
struct CredsFile {
    github_token: Option<String>,
    github_repo: Option<String>,
    codeberg_token: Option<String>,
    codeberg_repo: Option<String>,
    discord_token: Option<String>,
    discord_channels: Option<String>,
    nostr_nsec: Option<String>,
    nostr_relays: Option<String>,
    nostr_hashtag: Option<String>,
}

fn load_creds_into_env() {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".parseh").join("coord-creds.toml"),
        None => return,
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let creds: CredsFile = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: ignoring malformed {}: {e}", path.display());
            return;
        }
    };
    // Real env vars always win over the file.
    if std::env::var("PARSEH_COORD_GITHUB_TOKEN").is_err() {
        if let Some(t) = creds.github_token {
            std::env::set_var("PARSEH_COORD_GITHUB_TOKEN", t);
        }
    }
    if std::env::var("PARSEH_COORD_GITHUB_REPO").is_err() {
        if let Some(r) = creds.github_repo {
            std::env::set_var("PARSEH_COORD_GITHUB_REPO", r);
        }
    }
    // Codeberg: same env-over-file precedence as GitHub.
    if std::env::var("PARSEH_COORD_CODEBERG_TOKEN").is_err() {
        if let Some(t) = creds.codeberg_token {
            std::env::set_var("PARSEH_COORD_CODEBERG_TOKEN", t);
        }
    }
    if std::env::var("PARSEH_COORD_CODEBERG_REPO").is_err() {
        if let Some(r) = creds.codeberg_repo {
            std::env::set_var("PARSEH_COORD_CODEBERG_REPO", r);
        }
    }
    // Discord: same env-over-file precedence. The bot token is loaded into
    // the process env only (never echoed); the connector reads it there.
    if std::env::var("PARSEH_COORD_DISCORD_TOKEN").is_err() {
        if let Some(t) = creds.discord_token {
            std::env::set_var("PARSEH_COORD_DISCORD_TOKEN", t);
        }
    }
    if std::env::var("PARSEH_COORD_DISCORD_CHANNELS").is_err() {
        if let Some(c) = creds.discord_channels {
            std::env::set_var("PARSEH_COORD_DISCORD_CHANNELS", c);
        }
    }
    // Nostr: same env-over-file precedence. The nsec is loaded into the
    // process env only (never echoed); the connector reads it from there.
    if std::env::var("PARSEH_COORD_NOSTR_NSEC").is_err() {
        if let Some(n) = creds.nostr_nsec {
            std::env::set_var("PARSEH_COORD_NOSTR_NSEC", n);
        }
    }
    if std::env::var("PARSEH_COORD_NOSTR_RELAYS").is_err() {
        if let Some(r) = creds.nostr_relays {
            std::env::set_var("PARSEH_COORD_NOSTR_RELAYS", r);
        }
    }
    if std::env::var("PARSEH_COORD_NOSTR_HASHTAG").is_err() {
        if let Some(h) = creds.nostr_hashtag {
            std::env::set_var("PARSEH_COORD_NOSTR_HASHTAG", h);
        }
    }
}

#[derive(Debug, Deserialize)]
struct IssueSpec {
    title: String,
    body: String,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IssuesFile {
    #[serde(alias = "issue")]
    issues: Vec<IssueSpec>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    load_creds_into_env();
    let cli = Cli::parse();

    let db_path = match cli.db {
        Some(p) => p,
        None => store::default_db_path()?,
    };
    let store = Store::open(&db_path)?;

    // GitHub is real. The Nostr connector is also real but constructed
    // lazily — only for commands that actually touch Nostr — because
    // building it with no key configured GENERATES a fresh identity (and
    // prints the nsec once). We never trigger that for read-only commands
    // like `inbox`/`show`.
    let github = GithubConnector::from_env()?;
    // Codeberg is real and cheap to construct: from_env() never generates
    // any identity and never panics — if creds are absent, poll/post just
    // return a friendly error (the ingest loop reports + continues).
    let codeberg = CodebergConnector::from_env()?;
    // Discord is real and cheap to construct: from_env() never generates
    // any identity and never panics — if creds are absent, poll/post just
    // return a friendly error (the ingest loop reports + continues).
    let discord = DiscordConnector::from_env()?;

    match cli.cmd {
        Command::Ingest => {
            let nostr = NostrConnector::from_env()?;
            cmd_ingest(&store, &github, &codeberg, &discord, &nostr)
        }
        Command::Inbox { platform, limit } => cmd_inbox(&store, platform.as_deref(), limit),
        Command::Show { id } => cmd_show(&store, id),
        Command::Draft { id, body, stdin } => cmd_draft(&store, id, body, stdin),
        Command::Outbox => cmd_outbox(&store),
        Command::Approve { outbox_id } => cmd_approve(&store, outbox_id),
        Command::Send { outbox_id } => cmd_send(&store, outbox_id, &github, &codeberg, &discord),
        Command::BroadcastIssues {
            file,
            platform,
            dry_run,
        } => cmd_broadcast_issues(&github, &codeberg, &platform, &file, dry_run),
        Command::NostrLongform {
            file,
            title,
            dry_run,
        } => cmd_nostr_longform(&file, &title, dry_run),
    }
}

fn cmd_ingest(
    store: &Store,
    github: &GithubConnector,
    codeberg: &CodebergConnector,
    discord: &DiscordConnector,
    nostr: &NostrConnector,
) -> Result<()> {
    // GitHub + Codeberg + Discord + Nostr are real. Matrix is still an
    // honest stub: it bails loudly, we report and continue (the loop stays
    // uniform). Codeberg/Discord with no creds also bail loudly and are
    // reported the same way (no fake data, no panic).
    let stub_matrix = MatrixConnector;
    let connectors: Vec<&dyn Connector> =
        vec![github, codeberg, discord, nostr, &stub_matrix];

    let mut total_new = 0usize;
    let mut total_seen = 0usize;
    for c in connectors {
        match c.poll() {
            Ok(events) => {
                let mut new_here = 0usize;
                for ev in &events {
                    if store.upsert_event(ev)? {
                        new_here += 1;
                    }
                }
                total_seen += events.len();
                total_new += new_here;
                println!(
                    "[{}] polled {} item(s), {} new",
                    c.platform(),
                    events.len(),
                    new_here
                );
            }
            Err(e) => {
                println!("[{}] skipped: {}", c.platform(), e);
            }
        }
    }
    println!("ingest complete: {total_new} new of {total_seen} polled");
    Ok(())
}

fn cmd_inbox(store: &Store, platform: Option<&str>, limit: i64) -> Result<()> {
    let events = store.inbox(platform, limit)?;
    if events.is_empty() {
        println!("inbox empty (no unanswered events)");
        return Ok(());
    }
    let mut current = String::new();
    for e in &events {
        if e.platform != current {
            current = e.platform.clone();
            println!("\n== {} ==", current);
        }
        let snippet: String = e.body.chars().take(200).collect();
        let snippet = snippet.replace('\n', " ");
        println!(
            "#{:<5} [{}] @{}\n      {}\n      {}",
            e.id, e.kind, e.author, e.url, snippet
        );
    }
    println!("\n{} unanswered event(s)", events.len());
    Ok(())
}

fn cmd_show(store: &Store, id: i64) -> Result<()> {
    let e = store
        .get_event(id)?
        .with_context(|| format!("no event with id {id}"))?;
    println!("id:         {}", e.id);
    println!("platform:   {}", e.platform);
    println!("kind:       {}", e.kind);
    println!("thread_ref: {}", e.thread_ref);
    println!("author:     {}", e.author);
    println!("url:        {}", e.url);
    println!("created_at: {}", e.created_at);
    println!("ingested:   {}", e.ingested_at);
    println!("answered:   {}", e.answered);
    println!("---\n{}", e.body);
    Ok(())
}

fn cmd_draft(store: &Store, id: i64, body: Option<String>, stdin: bool) -> Result<()> {
    let event = store
        .get_event(id)?
        .with_context(|| format!("no event with id {id}"))?;
    let body = if stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading draft body from stdin")?;
        buf
    } else {
        body.context("provide --body <text> or --stdin")?
    };
    if body.trim().is_empty() {
        anyhow::bail!("refusing to store an empty draft");
    }
    let oid = store.create_draft(&event.platform, &event.thread_ref, &body, Some(event.id))?;
    println!(
        "drafted outbox #{oid} -> {} thread {} (status: draft).",
        event.platform, event.thread_ref
    );
    println!("next: parseh-coord approve {oid}   then   parseh-coord send {oid}");
    Ok(())
}

fn cmd_outbox(store: &Store) -> Result<()> {
    let entries = store.list_outbox()?;
    if entries.is_empty() {
        println!("outbox empty");
        return Ok(());
    }
    for e in &entries {
        let snippet: String = e.body.chars().take(120).collect();
        println!(
            "#{:<5} [{}] {} thread {} :: {}",
            e.id,
            e.status,
            e.platform,
            e.thread_ref,
            snippet.replace('\n', " ")
        );
        if let Some(err) = &e.error {
            println!("      last error: {err}");
        }
    }
    Ok(())
}

fn cmd_approve(store: &Store, outbox_id: i64) -> Result<()> {
    store.approve(outbox_id)?;
    println!("outbox #{outbox_id} approved. It can now be sent with: parseh-coord send {outbox_id}");
    Ok(())
}

fn cmd_send(
    store: &Store,
    outbox_id: i64,
    github: &GithubConnector,
    codeberg: &CodebergConnector,
    discord: &DiscordConnector,
) -> Result<()> {
    let entry = store
        .get_outbox(outbox_id)?
        .with_context(|| format!("no outbox entry with id {outbox_id}"))?;

    // The non-negotiable guard: ONLY approved entries are ever sent.
    if entry.status != OutboxStatus::Approved.as_str() {
        anyhow::bail!(
            "refusing to send outbox #{outbox_id}: status is '{}', not 'approved'. \
             Run `parseh-coord approve {outbox_id}` first. \
             parseh-coord never sends an unapproved message.",
            entry.status
        );
    }

    // The Nostr connector is built lazily and ONLY on the approved-send
    // path for a nostr entry — never speculatively (constructing it with
    // no key configured generates a fresh identity).
    let result = match entry.platform.as_str() {
        "github" => github.post(&entry.thread_ref, &entry.body),
        "codeberg" => codeberg.post(&entry.thread_ref, &entry.body),
        "discord" => discord.post(&entry.thread_ref, &entry.body),
        "matrix" => MatrixConnector.post(&entry.thread_ref, &entry.body),
        "nostr" => match NostrConnector::from_env() {
            Ok(n) => n.post(&entry.thread_ref, &entry.body),
            Err(e) => Err(e),
        },
        other => Err(anyhow::anyhow!("unknown platform {other:?}")),
    };

    match result {
        Ok(url) => {
            store.mark_sent(outbox_id)?;
            println!("sent outbox #{outbox_id} -> {url}");
            if let Some(eid) = entry.event_id {
                println!("event #{eid} marked answered");
            }
            Ok(())
        }
        Err(e) => {
            let msg = format!("{e:#}");
            store.mark_failed(outbox_id, &msg)?;
            anyhow::bail!("send failed for outbox #{outbox_id} (status set to 'failed'): {msg}");
        }
    }
}

fn cmd_broadcast_issues(
    github: &GithubConnector,
    codeberg: &CodebergConnector,
    platform: &str,
    file: &PathBuf,
    dry_run: bool,
) -> Result<()> {
    // The smallest honest seam: one `--platform` flag on the existing
    // subcommand (no parallel command). GitHub honours `labels`; Codeberg
    // (Forgejo) does not — its API wants integer label IDs, so issues are
    // created there WITHOUT labels (stated in the README "Limitations").
    let create: &dyn Fn(&str, &str, &[String]) -> Result<String> = match platform {
        "github" => &|t, b, l| github.create_issue(t, b, l),
        "codeberg" => &|t, b, l| codeberg.create_issue(t, b, l),
        other => anyhow::bail!(
            "unknown --platform {other:?}: expected 'github' or 'codeberg'"
        ),
    };

    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading issues file {}", file.display()))?;
    let parsed: IssuesFile = if file.extension().and_then(|e| e.to_str()) == Some("json") {
        serde_json::from_str(&text).context("parsing issues JSON")?
    } else {
        toml::from_str(&text).context("parsing issues TOML")?
    };
    if parsed.issues.is_empty() {
        anyhow::bail!("no issues found in {}", file.display());
    }
    println!(
        "{} issue(s) to broadcast to {}{}",
        parsed.issues.len(),
        platform,
        if dry_run { " (DRY RUN — no API calls)" } else { "" }
    );
    if platform == "codeberg" && !dry_run {
        eprintln!(
            "note: Codeberg (Forgejo) issue creation does not apply labels \
             (its API needs integer label IDs); issues are created without them."
        );
    }
    for (i, spec) in parsed.issues.iter().enumerate() {
        if dry_run {
            println!(
                "  [{}/{}] would create: {:?}  labels={:?}",
                i + 1,
                parsed.issues.len(),
                spec.title,
                spec.labels
            );
            continue;
        }
        match create(&spec.title, &spec.body, &spec.labels) {
            Ok(url) => println!("  [{}/{}] created: {url}", i + 1, parsed.issues.len()),
            Err(e) => {
                // Report and continue so a single failure doesn't strand
                // the rest; the operator sees exactly which failed.
                eprintln!(
                    "  [{}/{}] FAILED {:?}: {e:#}",
                    i + 1,
                    parsed.issues.len(),
                    spec.title
                );
            }
        }
    }
    Ok(())
}

fn cmd_nostr_longform(file: &PathBuf, title: &str, dry_run: bool) -> Result<()> {
    let markdown = std::fs::read_to_string(file)
        .with_context(|| format!("reading long-form markdown file {}", file.display()))?;
    if markdown.trim().is_empty() {
        anyhow::bail!("refusing to publish an empty long-form article");
    }
    if title.trim().is_empty() {
        anyhow::bail!("--title is required for a NIP-23 article");
    }
    if dry_run {
        println!(
            "DRY RUN — would publish NIP-23 long-form article (no relay contacted):\n  \
             title: {title:?}\n  body:  {} byte(s) from {}",
            markdown.len(),
            file.display()
        );
        return Ok(());
    }
    // Constructing the connector here may generate + print a fresh nsec if
    // none is configured (documented). This is an explicit operator command.
    let nostr = NostrConnector::from_env()?;
    let url = nostr
        .post_longform(title, &markdown)
        .context("publishing the NIP-23 long-form article")?;
    println!("published long-form article -> {url}");
    println!("(best-effort across public relays — delivery is not guaranteed)");
    Ok(())
}
