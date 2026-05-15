//! Local SQLite store for parseh-coord.
//!
//! Two tables:
//!  - `event`  : ingested community activity (issues, comments, …). Append-only
//!               in spirit; the only mutation is flipping `answered` to 1.
//!  - `outbox` : operator-drafted replies / new posts. State machine is
//!               draft → approved → sent (or → failed). A draft can NEVER be
//!               sent without an explicit `approve` first — this is enforced
//!               here in `mark_sent`/`mark_failed` callers and in the CLI.
//!
//! Database lives at `~/.parseh/coord.db`. The `~/.parseh/` directory holds
//! local-only operator state and credentials and is gitignored at the repo
//! root (the `*.sqlite` / `data/` / `db/` patterns plus an explicit
//! `.parseh/`-style note — the file itself lives outside the repo anyway).

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// An event as produced by a connector's `poll()`, before it is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestEvent {
    pub platform: String,
    /// e.g. "issue", "issue_comment", "discussion_comment".
    pub kind: String,
    /// Stable reference to the thread this belongs to (e.g. issue number
    /// "42", or "discussion:7"). Used by `post()` to know where to reply.
    pub thread_ref: String,
    pub author: String,
    pub body: String,
    pub url: String,
    /// Unix seconds — when the upstream item was created.
    pub created_at: i64,
}

/// A persisted event, as read back from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub id: i64,
    pub platform: String,
    pub kind: String,
    pub thread_ref: String,
    pub author: String,
    pub body: String,
    pub url: String,
    pub created_at: i64,
    pub ingested_at: i64,
    pub answered: bool,
}

/// Outbox status state machine. `Sent`/`Failed` are terminal states the
/// store writes via string literals; the enum is the canonical reference
/// and is exercised by `parse`/`as_str` round-trip tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutboxStatus {
    Draft,
    Approved,
    Sent,
    Failed,
}

impl OutboxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutboxStatus::Draft => "draft",
            OutboxStatus::Approved => "approved",
            OutboxStatus::Sent => "sent",
            OutboxStatus::Failed => "failed",
        }
    }

    // Used by tests + available for future status-driven CLI filters.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "draft" => OutboxStatus::Draft,
            "approved" => OutboxStatus::Approved,
            "sent" => OutboxStatus::Sent,
            "failed" => OutboxStatus::Failed,
            other => anyhow::bail!("unknown outbox status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: i64,
    pub platform: String,
    pub thread_ref: String,
    pub body: String,
    pub drafted_at: i64,
    pub sent_at: Option<i64>,
    pub status: String,
    /// Set when status == failed.
    pub error: Option<String>,
    /// The event this draft is linked to (for marking it answered on send).
    pub event_id: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

/// Returns the default `~/.parseh/coord.db` path, creating `~/.parseh/`.
pub fn default_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let dir = home.join(".parseh");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir.join("coord.db"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        let s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    /// In-memory store for tests.
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS event (
                id          INTEGER PRIMARY KEY,
                platform    TEXT NOT NULL,
                kind        TEXT NOT NULL,
                thread_ref  TEXT NOT NULL,
                author      TEXT NOT NULL,
                body        TEXT NOT NULL,
                url         TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                ingested_at INTEGER NOT NULL,
                answered    INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS outbox (
                id         INTEGER PRIMARY KEY,
                platform   TEXT NOT NULL,
                thread_ref TEXT NOT NULL,
                body       TEXT NOT NULL,
                drafted_at INTEGER NOT NULL,
                sent_at    INTEGER,
                status     TEXT NOT NULL,
                error      TEXT,
                event_id   INTEGER REFERENCES event(id)
            );

            CREATE INDEX IF NOT EXISTS idx_event_answered_created
                ON event(answered, created_at);
            CREATE INDEX IF NOT EXISTS idx_event_platform_thread
                ON event(platform, thread_ref);

            -- Dedupe key for ingest: a single upstream item is uniquely
            -- (platform, thread_ref, author, created_at).
            CREATE UNIQUE INDEX IF NOT EXISTS uq_event_dedupe
                ON event(platform, thread_ref, author, created_at);
            "#,
        )?;
        Ok(())
    }

    /// Insert an event, ignoring it if it already exists (dedupe by
    /// `(platform, thread_ref, author, created_at)`).
    ///
    /// Returns `true` if a new row was inserted, `false` if it was a dup.
    pub fn upsert_event(&self, ev: &IngestEvent) -> Result<bool> {
        let changed = self.conn.execute(
            r#"
            INSERT OR IGNORE INTO event
                (platform, kind, thread_ref, author, body, url, created_at, ingested_at, answered)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
            "#,
            params![
                ev.platform,
                ev.kind,
                ev.thread_ref,
                ev.author,
                ev.body,
                ev.url,
                ev.created_at,
                now(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// Unanswered events, newest-first, optionally filtered by platform.
    pub fn inbox(&self, platform: Option<&str>, limit: i64) -> Result<Vec<StoredEvent>> {
        let mut out = Vec::new();
        if let Some(p) = platform {
            let mut stmt = self.conn.prepare(
                "SELECT id, platform, kind, thread_ref, author, body, url, created_at, ingested_at, answered
                 FROM event WHERE answered = 0 AND platform = ?1
                 ORDER BY created_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![p, limit], Self::map_event)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, platform, kind, thread_ref, author, body, url, created_at, ingested_at, answered
                 FROM event WHERE answered = 0
                 ORDER BY created_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], Self::map_event)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn get_event(&self, id: i64) -> Result<Option<StoredEvent>> {
        let r = self
            .conn
            .query_row(
                "SELECT id, platform, kind, thread_ref, author, body, url, created_at, ingested_at, answered
                 FROM event WHERE id = ?1",
                params![id],
                Self::map_event,
            )
            .optional()?;
        Ok(r)
    }

    fn map_event(row: &rusqlite::Row) -> rusqlite::Result<StoredEvent> {
        Ok(StoredEvent {
            id: row.get(0)?,
            platform: row.get(1)?,
            kind: row.get(2)?,
            thread_ref: row.get(3)?,
            author: row.get(4)?,
            body: row.get(5)?,
            url: row.get(6)?,
            created_at: row.get(7)?,
            ingested_at: row.get(8)?,
            answered: row.get::<_, i64>(9)? != 0,
        })
    }

    pub fn mark_answered(&self, event_id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE event SET answered = 1 WHERE id = ?1", params![event_id])?;
        Ok(())
    }

    /// Create a draft in the outbox linked to an event's thread.
    pub fn create_draft(
        &self,
        platform: &str,
        thread_ref: &str,
        body: &str,
        event_id: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO outbox (platform, thread_ref, body, drafted_at, sent_at, status, error, event_id)
            VALUES (?1, ?2, ?3, ?4, NULL, 'draft', NULL, ?5)
            "#,
            params![platform, thread_ref, body, now(), event_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_outbox(&self, id: i64) -> Result<Option<OutboxEntry>> {
        let r = self
            .conn
            .query_row(
                "SELECT id, platform, thread_ref, body, drafted_at, sent_at, status, error, event_id
                 FROM outbox WHERE id = ?1",
                params![id],
                Self::map_outbox,
            )
            .optional()?;
        Ok(r)
    }

    /// All outbox entries that are still actionable (draft or approved),
    /// plus recently-failed ones, newest-first.
    pub fn list_outbox(&self) -> Result<Vec<OutboxEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, platform, thread_ref, body, drafted_at, sent_at, status, error, event_id
             FROM outbox ORDER BY drafted_at DESC",
        )?;
        let rows = stmt.query_map([], Self::map_outbox)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    fn map_outbox(row: &rusqlite::Row) -> rusqlite::Result<OutboxEntry> {
        Ok(OutboxEntry {
            id: row.get(0)?,
            platform: row.get(1)?,
            thread_ref: row.get(2)?,
            body: row.get(3)?,
            drafted_at: row.get(4)?,
            sent_at: row.get(5)?,
            status: row.get(6)?,
            error: row.get(7)?,
            event_id: row.get(8)?,
        })
    }

    /// Flip draft → approved. Errors if the entry is not currently a draft.
    pub fn approve(&self, outbox_id: i64) -> Result<()> {
        let entry = self
            .get_outbox(outbox_id)?
            .with_context(|| format!("no outbox entry with id {outbox_id}"))?;
        if entry.status != OutboxStatus::Draft.as_str() {
            anyhow::bail!(
                "outbox #{outbox_id} is '{}', only a 'draft' can be approved",
                entry.status
            );
        }
        self.conn.execute(
            "UPDATE outbox SET status = 'approved' WHERE id = ?1",
            params![outbox_id],
        )?;
        Ok(())
    }

    /// Mark an outbox entry sent. Also marks the linked event answered.
    /// Caller MUST have verified status == approved first (the CLI does;
    /// this is the second guard).
    pub fn mark_sent(&self, outbox_id: i64) -> Result<()> {
        let entry = self
            .get_outbox(outbox_id)?
            .with_context(|| format!("no outbox entry with id {outbox_id}"))?;
        if entry.status != OutboxStatus::Approved.as_str() {
            anyhow::bail!(
                "refusing to mark #{outbox_id} sent: status is '{}', not 'approved'",
                entry.status
            );
        }
        self.conn.execute(
            "UPDATE outbox SET status = 'sent', sent_at = ?2, error = NULL WHERE id = ?1",
            params![outbox_id, now()],
        )?;
        if let Some(eid) = entry.event_id {
            self.mark_answered(eid)?;
        }
        Ok(())
    }

    /// Record a send failure (status → failed, store the error string).
    pub fn mark_failed(&self, outbox_id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET status = 'failed', error = ?2 WHERE id = ?1",
            params![outbox_id, error],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(author: &str, ts: i64) -> IngestEvent {
        IngestEvent {
            platform: "github".into(),
            kind: "issue".into(),
            thread_ref: "42".into(),
            author: author.into(),
            body: "hello world".into(),
            url: "https://example/42".into(),
            created_at: ts,
        }
    }

    #[test]
    fn insert_and_dedupe() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.upsert_event(&ev("alice", 100)).unwrap(), "first insert is new");
        assert!(
            !s.upsert_event(&ev("alice", 100)).unwrap(),
            "same (platform,thread,author,created) is a dup"
        );
        // Different author at same time => new row.
        assert!(s.upsert_event(&ev("bob", 100)).unwrap());
        // Same author, different time => new row.
        assert!(s.upsert_event(&ev("alice", 200)).unwrap());
        let inbox = s.inbox(None, 100).unwrap();
        assert_eq!(inbox.len(), 3);
    }

    #[test]
    fn inbox_newest_first_and_platform_filter() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_event(&ev("alice", 100)).unwrap();
        s.upsert_event(&ev("alice", 300)).unwrap();
        s.upsert_event(&ev("alice", 200)).unwrap();
        let mut other = ev("carol", 250);
        other.platform = "matrix".into();
        s.upsert_event(&other).unwrap();

        let all = s.inbox(None, 100).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].created_at, 300, "newest first");
        assert_eq!(all[1].created_at, 250);

        let gh = s.inbox(Some("github"), 100).unwrap();
        assert_eq!(gh.len(), 3);
        assert!(gh.iter().all(|e| e.platform == "github"));

        let limited = s.inbox(None, 2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn answered_flag_hides_from_inbox() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_event(&ev("alice", 100)).unwrap();
        let e = &s.inbox(None, 10).unwrap()[0];
        let id = e.id;
        s.mark_answered(id).unwrap();
        assert!(s.inbox(None, 10).unwrap().is_empty());
        // The event still exists, just answered.
        assert!(s.get_event(id).unwrap().unwrap().answered);
    }

    #[test]
    fn outbox_state_machine_happy_path() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_event(&ev("alice", 100)).unwrap();
        let eid = s.inbox(None, 10).unwrap()[0].id;

        let oid = s.create_draft("github", "42", "thanks for filing", Some(eid)).unwrap();
        assert_eq!(s.get_outbox(oid).unwrap().unwrap().status, "draft");

        s.approve(oid).unwrap();
        assert_eq!(s.get_outbox(oid).unwrap().unwrap().status, "approved");

        s.mark_sent(oid).unwrap();
        let entry = s.get_outbox(oid).unwrap().unwrap();
        assert_eq!(entry.status, "sent");
        assert!(entry.sent_at.is_some());
        // Linked event got marked answered.
        assert!(s.get_event(eid).unwrap().unwrap().answered);
    }

    #[test]
    fn cannot_mark_sent_without_approval() {
        let s = Store::open_in_memory().unwrap();
        let oid = s.create_draft("github", "42", "body", None).unwrap();
        // Skip approve — go straight to mark_sent.
        let err = s.mark_sent(oid).unwrap_err().to_string();
        assert!(err.contains("not 'approved'"), "got: {err}");
        assert_eq!(s.get_outbox(oid).unwrap().unwrap().status, "draft");
    }

    #[test]
    fn cannot_approve_non_draft() {
        let s = Store::open_in_memory().unwrap();
        let oid = s.create_draft("github", "42", "body", None).unwrap();
        s.approve(oid).unwrap();
        // Second approve must fail (it's already approved, not a draft).
        let err = s.approve(oid).unwrap_err().to_string();
        assert!(err.contains("only a 'draft' can be approved"), "got: {err}");
    }

    #[test]
    fn mark_failed_records_error() {
        let s = Store::open_in_memory().unwrap();
        let oid = s.create_draft("github", "42", "body", None).unwrap();
        s.approve(oid).unwrap();
        s.mark_failed(oid, "HTTP 403 forbidden").unwrap();
        let e = s.get_outbox(oid).unwrap().unwrap();
        assert_eq!(e.status, "failed");
        assert_eq!(e.error.as_deref(), Some("HTTP 403 forbidden"));
    }

    #[test]
    fn status_parse_roundtrip() {
        for st in [
            OutboxStatus::Draft,
            OutboxStatus::Approved,
            OutboxStatus::Sent,
            OutboxStatus::Failed,
        ] {
            assert_eq!(OutboxStatus::parse(st.as_str()).unwrap().as_str(), st.as_str());
        }
        assert!(OutboxStatus::parse("nope").is_err());
    }
}
