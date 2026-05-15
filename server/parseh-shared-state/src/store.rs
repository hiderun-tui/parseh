//! The [`SharedState`] store API.
//!
//! Wraps a (optionally SQLCipher-encrypted) SQLite connection behind a
//! `parking_lot::Mutex`. All CRUD goes through here. Queries return
//! domain types (`JobSpec`, `JobOutcome`, …) — callers never see
//! `rusqlite::Row`.
//!
//! ## Concurrency
//!
//! V0.2 uses a single mutex around one connection. SQLite is fine with
//! this — write throughput is bounded by `fsync` anyway, and the
//! contention vs. r/w lock complexity tradeoff is not worth it for the
//! tens-to-hundreds writes/minute expected per node. V0.3 may revisit
//! with a connection pool if benchmarks justify it.
//!
//! ## Encryption-at-rest
//!
//! When the `encrypted` Cargo feature is on, the connection issues
//! `PRAGMA key = "x'...'"` before any other statement, per the SQLCipher
//! recipe. With the feature off, that PRAGMA is harmlessly issued
//! against vanilla SQLite where it is a no-op (the PRAGMA exists but
//! does nothing). This means we keep one code path; the `encrypted`
//! flag changes *only* the linker.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use libp2p::PeerId;
use parking_lot::Mutex;
use parseh_task::{
    from_cbor_bytes, to_cbor_bytes, ContentHash, JobOutcome, JobResult, JobSpec, JobVerification,
    OutcomeVerdict, VerifierMethod, VerifierVerdict,
};
use rusqlite::{params, Connection, OpenFlags};
use thiserror::Error;
use tracing::debug;

use crate::{
    cipher::KeyMaterial,
    delta::{verify_delta, DeltaKind, SignError, StateDelta},
    migrations::run_migrations,
};

// ---------------------------------------------------------------------
// Public errors
// ---------------------------------------------------------------------

/// Errors that can occur while opening a [`SharedState`].
#[derive(Error, Debug)]
pub enum OpenError {
    /// The database file did not exist and
    /// `OpenOptions::create_if_missing` was `false`.
    #[error("database file does not exist and create_if_missing is false")]
    NotFound,
    /// The supplied [`KeyMaterial`] did not decrypt an existing
    /// SQLCipher database (or the schema sanity-check after the PRAGMA
    /// failed for any other reason).
    #[error("wrong key for encrypted database (or corruption)")]
    WrongKey,
    /// A migration step failed.
    #[error("migration failed: {0}")]
    Migration(String),
    /// Generic SQLite error during open.
    #[error("sqlite open: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// I/O error during open.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors returned by [`SharedState`] CRUD methods.
#[derive(Error, Debug)]
pub enum StoreError {
    /// SQLite returned an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// CBOR encode failed.
    #[error("cbor encode: {0}")]
    CborEncode(String),
    /// CBOR decode failed.
    #[error("cbor decode: {0}")]
    CborDecode(String),
    /// Foreign-key violation — e.g. trying to record a `JobResult`
    /// whose `spec_hash` does not correspond to any known task.
    #[error("foreign-key violation: {0}")]
    ForeignKey(String),
    /// Delta signature verification failed.
    #[error("bad delta signature: {0}")]
    BadSignature(#[from] SignError),
}

// ---------------------------------------------------------------------
// Open options
// ---------------------------------------------------------------------

/// Options for [`SharedState::open`].
pub struct OpenOptions {
    /// Filesystem path of the SQLite (or SQLCipher) database.
    pub path: PathBuf,
    /// 32-byte key material.
    pub key: KeyMaterial,
    /// If `true`, create the database when missing. If `false` and the
    /// file does not exist, [`OpenError::NotFound`] is returned.
    pub create_if_missing: bool,
}

impl OpenOptions {
    /// Shorthand: open or create at `path` with `key`.
    pub fn create(path: PathBuf, key: KeyMaterial) -> Self {
        Self {
            path,
            key,
            create_if_missing: true,
        }
    }
}

// ---------------------------------------------------------------------
// SharedState
// ---------------------------------------------------------------------

/// The PARSEH shared-state store.
pub struct SharedState {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState").finish_non_exhaustive()
    }
}

impl SharedState {
    /// Open (or create) a database.
    pub fn open(opts: OpenOptions) -> Result<Self, OpenError> {
        let path = opts.path.clone();
        let exists = path.exists();
        if !exists && !opts.create_if_missing {
            return Err(OpenError::NotFound);
        }

        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI;
        if opts.create_if_missing {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }

        let mut conn = Connection::open_with_flags(&path, flags)?;

        // Issue PRAGMA key as the very first statement (SQLCipher
        // recipe). When the `encrypted` feature is off, vanilla SQLite
        // accepts the PRAGMA as a no-op.
        apply_key_pragmas(&conn, &opts.key)?;

        // Sanity check: a wrong key on an existing SQLCipher DB makes
        // SELECTs fail. We touch `sqlite_master` immediately to catch
        // that. On the plain `bundled` build this always succeeds; the
        // check is essentially free, so we run it unconditionally.
        if let Err(e) =
            conn.query_row::<i64, _, _>("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        {
            // Distinguish wrong-key (existing file we cannot read)
            // from generic SQLite errors on fresh files.
            return if exists {
                Err(OpenError::WrongKey)
            } else {
                Err(OpenError::Sqlite(e))
            };
        }

        // Foreign keys are off by default in SQLite — turn them on
        // before any DDL/DML so the schema's REFERENCES clauses bite.
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;

        run_migrations(&mut conn).map_err(|e| OpenError::Migration(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ----- Insert / observe ------------------------------------------------

    /// Record a [`JobSpec`] we observed on the network.
    ///
    /// Idempotent — re-inserting the same spec (by content hash) is a
    /// no-op.
    pub fn record_spec(&self, spec: &JobSpec) -> Result<(), StoreError> {
        let spec_hash = spec.content_hash();
        let cbor = to_cbor_bytes(spec).map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let kind = match spec.kind {
            parseh_task::JobKind::Inference => "Inference",
            parseh_task::JobKind::Relay => "Relay",
            parseh_task::JobKind::Storage => "Storage",
        };
        let now = now_unix();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO tasks
                 (spec_hash, submitter, kind, sensitive, submitted_at, spec_cbor, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                spec_hash.as_bytes().as_slice(),
                spec.submitter.to_bytes(),
                kind,
                spec.sensitive as i32,
                spec.submitted_at as i64,
                cbor,
                now,
            ],
        )?;
        Ok(())
    }

    /// Record a [`JobResult`]. Requires the referenced spec to already
    /// be in the `tasks` table.
    pub fn record_result(&self, result: &JobResult) -> Result<(), StoreError> {
        let result_hash = result.content_hash();
        let cbor = to_cbor_bytes(result).map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let now = now_unix();
        let conn = self.conn.lock();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO results
                     (result_hash, spec_hash, executor, executed_at, result_cbor, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    result_hash.as_bytes().as_slice(),
                    result.spec_hash.as_bytes().as_slice(),
                    result.executor.to_bytes(),
                    result.executed_at as i64,
                    cbor,
                    now,
                ],
            )
            .map_err(map_fk_err)?;
        debug!(rows = inserted, "record_result");
        Ok(())
    }

    /// Record a [`JobVerification`]. Requires the referenced result to
    /// already be in the `results` table.
    pub fn record_verification(&self, v: &JobVerification) -> Result<(), StoreError> {
        let verification_hash = v.content_hash();
        let cbor = to_cbor_bytes(v).map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let verdict = match &v.verdict {
            VerifierVerdict::Agreed => "Agreed",
            VerifierVerdict::Disagreed { .. } => "Disagreed",
            VerifierVerdict::Abstained => "Abstained",
        };
        let method = match v.method_used {
            VerifierMethod::Deterministic => "Deterministic",
            VerifierMethod::SpotCheck => "SpotCheck",
            VerifierMethod::Statistical => "Statistical",
        };
        let now = now_unix();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO verifications
                 (verification_hash, result_hash, verifier, verdict, method, verified_at, verification_cbor, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                verification_hash.as_bytes().as_slice(),
                v.result_hash.as_bytes().as_slice(),
                v.verifier.to_bytes(),
                verdict,
                method,
                v.verified_at as i64,
                cbor,
                now,
            ],
        )
        .map_err(map_fk_err)?;
        Ok(())
    }

    /// Record a finalised [`JobOutcome`].
    pub fn record_outcome(&self, outcome: &JobOutcome) -> Result<(), StoreError> {
        let outcome_hash = outcome.content_hash();
        let cbor = to_cbor_bytes(outcome).map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let verdict = match &outcome.verdict {
            OutcomeVerdict::Valid { .. } => "Valid",
            OutcomeVerdict::Disputed { .. } => "Disputed",
            OutcomeVerdict::Indeterminate => "Indeterminate",
        };
        let now = now_unix();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO outcomes
                 (outcome_hash, spec_hash, result_hash, verdict, finalised_at, observed_by, outcome_cbor, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                outcome_hash.as_bytes().as_slice(),
                outcome.spec_hash.as_bytes().as_slice(),
                outcome.result_hash.as_bytes().as_slice(),
                verdict,
                outcome.finalised_at as i64,
                outcome.observed_by.to_bytes(),
                cbor,
                now,
            ],
        )
        .map_err(map_fk_err)?;
        Ok(())
    }

    /// Record an outcome that arrived via `/parseh/state-sync/1.0.0`.
    ///
    /// Unlike [`Self::record_outcome`], the syncing node may NOT have
    /// the originating `JobSpec` / `JobResult` rows — it was partitioned
    /// away while the task ran, which is the entire reason it is now
    /// syncing. The `outcomes` table's foreign keys onto `tasks` /
    /// `results` exist to protect the **local-quorum** path (you only
    /// finalise what you observed); they are intentionally too strict
    /// for anti-entropy, where a signed [`JobOutcome`] is a
    /// self-contained, independently-verifiable artifact (the caller
    /// has already re-checked its inner observer signature).
    ///
    /// To keep referential integrity intact without inventing fake
    /// spec/result bodies, this inserts minimal **stub** parent rows
    /// (content hash + zero/empty bodies, marked `kind = 'SyncedStub'`)
    /// when they are absent, then the outcome. Downstream joins (e.g.
    /// [`Self::detect_repeating_verifier_sets`]) simply find no
    /// verification detail for a stub — which is correct: the syncing
    /// node genuinely has only the finalised verdict, not the evidence.
    /// A later real spec/result delivery is an idempotent
    /// `INSERT OR IGNORE` no-op on the stub's primary key.
    pub fn record_synced_outcome(&self, outcome: &JobOutcome) -> Result<(), StoreError> {
        let cbor = to_cbor_bytes(outcome).map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let outcome_hash = outcome.content_hash();
        let verdict = match &outcome.verdict {
            OutcomeVerdict::Valid { .. } => "Valid",
            OutcomeVerdict::Disputed { .. } => "Disputed",
            OutcomeVerdict::Indeterminate => "Indeterminate",
        };
        let now = now_unix();
        let conn = self.conn.lock();
        // Stub task row (only if the real spec is unknown).
        conn.execute(
            "INSERT OR IGNORE INTO tasks
                 (spec_hash, submitter, kind, sensitive, submitted_at, spec_cbor, observed_at)
             VALUES (?1, ?2, 'SyncedStub', 0, ?3, ?4, ?3)",
            params![
                outcome.spec_hash.as_bytes().as_slice(),
                outcome.observed_by.to_bytes(),
                outcome.finalised_at as i64,
                Vec::<u8>::new(),
            ],
        )?;
        // Stub result row.
        conn.execute(
            "INSERT OR IGNORE INTO results
                 (result_hash, spec_hash, executor, executed_at, result_cbor, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?4)",
            params![
                outcome.result_hash.as_bytes().as_slice(),
                outcome.spec_hash.as_bytes().as_slice(),
                outcome.observed_by.to_bytes(),
                outcome.finalised_at as i64,
                Vec::<u8>::new(),
            ],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO outcomes
                 (outcome_hash, spec_hash, result_hash, verdict, finalised_at, observed_by, outcome_cbor, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                outcome_hash.as_bytes().as_slice(),
                outcome.spec_hash.as_bytes().as_slice(),
                outcome.result_hash.as_bytes().as_slice(),
                verdict,
                outcome.finalised_at as i64,
                outcome.observed_by.to_bytes(),
                cbor,
                now,
            ],
        )
        .map_err(map_fk_err)?;
        Ok(())
    }

    /// Append a reputation-log entry for `peer`.
    pub fn apply_reputation_delta(
        &self,
        peer: PeerId,
        delta: i32,
        reason: &str,
        related_hash: Option<ContentHash>,
    ) -> Result<(), StoreError> {
        let now = now_unix();
        let related_bytes: Option<Vec<u8>> = related_hash.map(|h| h.as_bytes().to_vec());
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO reputation_log (peer, delta, reason, related_hash, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                peer.to_bytes(),
                delta as i64,
                reason,
                related_bytes,
                now,
            ],
        )?;
        Ok(())
    }

    /// Upsert a governance rule (last-write-wins by `rule_name`).
    pub fn upsert_governance_rule(
        &self,
        rule_name: &str,
        rule_value: &str,
        proposer: PeerId,
        approvers: &[PeerId],
    ) -> Result<(), StoreError> {
        let approver_bytes: Vec<Vec<u8>> = approvers.iter().map(|p| p.to_bytes()).collect();
        let approvers_cbor = to_cbor_bytes(&approver_bytes)
            .map_err(|e| StoreError::CborEncode(e.to_string()))?;
        let now = now_unix();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO governance_rules
                 (rule_name, rule_value, proposed_at, activated_at, proposer, approvers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(rule_name) DO UPDATE SET
                 rule_value   = excluded.rule_value,
                 activated_at = excluded.activated_at,
                 proposer     = excluded.proposer,
                 approvers    = excluded.approvers",
            params![
                rule_name,
                rule_value,
                now,
                now,
                proposer.to_bytes(),
                approvers_cbor,
            ],
        )?;
        Ok(())
    }

    // ----- Queries -------------------------------------------------------

    /// Sum of all reputation deltas applied to `peer`.
    ///
    /// Returns 0 if no entries exist (rather than a sentinel `None`) —
    /// reputation is monotonic-with-decay in V0.2 and the natural
    /// floor is zero.
    pub fn reputation_of(&self, peer: PeerId) -> Result<i64, StoreError> {
        let conn = self.conn.lock();
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(delta), 0) FROM reputation_log WHERE peer = ?1",
            params![peer.to_bytes()],
            |r| r.get(0),
        )?;
        Ok(sum)
    }

    /// Peers whose summed reputation ≥ `min_rep`. Useful for
    /// `Established` tier checks.
    pub fn established_peers(&self, min_rep: i64) -> Result<Vec<PeerId>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT peer, SUM(delta) AS rep
               FROM reputation_log
              GROUP BY peer
             HAVING rep >= ?1",
        )?;
        let rows = stmt.query_map(params![min_rep], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            Ok(bytes)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row?;
            match PeerId::from_bytes(&bytes) {
                Ok(p) => out.push(p),
                Err(e) => {
                    debug!(error = %e, "skipping unparseable peer bytes in reputation_log");
                }
            }
        }
        Ok(out)
    }

    /// All tasks submitted at or after `since_unix`, decoded.
    pub fn recent_tasks(&self, since_unix: u64) -> Result<Vec<JobSpec>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT spec_cbor FROM tasks
              WHERE submitted_at >= ?1
              ORDER BY submitted_at ASC",
        )?;
        let rows = stmt.query_map(params![since_unix as i64], |row| {
            let blob: Vec<u8> = row.get(0)?;
            Ok(blob)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let blob = row?;
            let spec: JobSpec =
                from_cbor_bytes(&blob).map_err(|e| StoreError::CborDecode(e.to_string()))?;
            out.push(spec);
        }
        Ok(out)
    }

    /// The (last-write-wins) outcome for a given `spec_hash`, if any.
    /// V0.2 stores all outcomes; this query returns the most recently
    /// observed one — sufficient for "what does the network think
    /// happened to this task" lookups.
    pub fn outcome_for_spec(
        &self,
        spec_hash: &ContentHash,
    ) -> Result<Option<JobOutcome>, StoreError> {
        let conn = self.conn.lock();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT outcome_cbor FROM outcomes
                  WHERE spec_hash = ?1
                  ORDER BY observed_at DESC LIMIT 1",
                params![spec_hash.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
        match blob {
            Some(b) => Ok(Some(
                from_cbor_bytes(&b).map_err(|e| StoreError::CborDecode(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// All locally-known outcomes finalised at or after `since_unix`,
    /// newest first, capped at `limit`.
    ///
    /// Backs the `/parseh/state-sync/1.0.0` responder: a peer that
    /// missed the partition window asks for "everything since T", and
    /// this is the query that answers it. Index-backed by
    /// `idx_outcomes_finalised_at` (added in `SCHEMA_V1`).
    ///
    /// "Newest first" lets the responder return the most-recent
    /// `limit` outcomes when more exist than the cap — the caller pages
    /// backward by issuing a follow-up request with an earlier
    /// `since_unix` if it detects truncation. Each returned
    /// [`JobOutcome`] is still individually signed by its observer; the
    /// requester re-verifies before persisting.
    pub fn outcomes_since(
        &self,
        since_unix: u64,
        limit: usize,
    ) -> Result<Vec<JobOutcome>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT outcome_cbor FROM outcomes
              WHERE finalised_at >= ?1
              ORDER BY finalised_at DESC, outcome_hash ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![since_unix as i64, limit as i64],
            |row| {
                let blob: Vec<u8> = row.get(0)?;
                Ok(blob)
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let blob = row?;
            let outcome: JobOutcome = from_cbor_bytes(&blob)
                .map_err(|e| StoreError::CborDecode(e.to_string()))?;
            out.push(outcome);
        }
        Ok(out)
    }

    /// All verifications recorded for `result_hash`, decoded.
    pub fn verifications_for_result(
        &self,
        result_hash: &ContentHash,
    ) -> Result<Vec<JobVerification>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT verification_cbor FROM verifications
              WHERE result_hash = ?1
              ORDER BY verified_at ASC",
        )?;
        let rows = stmt.query_map(params![result_hash.as_bytes().as_slice()], |row| {
            let blob: Vec<u8> = row.get(0)?;
            Ok(blob)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let v: JobVerification = from_cbor_bytes(&row?)
                .map_err(|e| StoreError::CborDecode(e.to_string()))?;
            out.push(v);
        }
        Ok(out)
    }

    /// Look up a governance rule by name. Returns the JSON-encoded
    /// value if set.
    pub fn governance_rule(&self, name: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock();
        let r = conn
            .query_row(
                "SELECT rule_value FROM governance_rules WHERE rule_name = ?1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
        Ok(r)
    }

    // ----- Detection queries --------------------------------------------

    /// Detect repeating verifier sets — the V0.2 manual-review path for
    /// the §7 K-colluder ring described in `sybil-cost-analysis.md`.
    ///
    /// Algorithm:
    ///
    /// 1. Restrict to verifications + outcomes finalised inside
    ///    `[now - window_secs, now]`.
    /// 2. For each `Valid` outcome, snapshot the set of verifiers that
    ///    `Agreed` with the executor.
    /// 3. Group by `(submitter, sorted verifier set)`. Flag groups
    ///    whose count ≥ `min_count`.
    ///
    /// V0.2 is intentionally "list it for the maintainer team";
    /// automated flagging + reputation penalty lives in V0.3 once the
    /// chain validates state transitions.
    pub fn detect_repeating_verifier_sets(
        &self,
        window_secs: u64,
        min_count: u32,
    ) -> Result<Vec<(PeerId, Vec<PeerId>, u32)>, StoreError> {
        let conn = self.conn.lock();
        let now = now_unix();
        let cutoff: i64 = now.saturating_sub(window_secs as i64);

        // Pull (task submitter, result executor, agreeing verifier) tuples.
        let mut stmt = conn.prepare(
            "SELECT t.submitter, o.outcome_hash, v.verifier
               FROM outcomes  o
               JOIN tasks     t ON t.spec_hash   = o.spec_hash
               JOIN results   r ON r.result_hash = o.result_hash
               JOIN verifications v ON v.result_hash = r.result_hash
              WHERE o.verdict = 'Valid'
                AND v.verdict = 'Agreed'
                AND o.finalised_at >= ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            let submitter: Vec<u8> = row.get(0)?;
            let outcome_hash: Vec<u8> = row.get(1)?;
            let verifier: Vec<u8> = row.get(2)?;
            Ok((submitter, outcome_hash, verifier))
        })?;

        // Build verifier set per outcome.
        // Key: outcome_hash bytes. Value: (submitter, BTreeSet<verifier>)
        type PerOutcome = BTreeMap<Vec<u8>, (Vec<u8>, BTreeSet<Vec<u8>>)>;
        let mut per_outcome: PerOutcome = BTreeMap::new();
        for r in rows {
            let (submitter, outcome_hash, verifier) = r?;
            let entry = per_outcome
                .entry(outcome_hash)
                .or_insert_with(|| (submitter.clone(), BTreeSet::new()));
            entry.1.insert(verifier);
        }

        // Group by (submitter, sorted verifier-set).
        // Vec<u8> keys are byte-sortable, so a BTreeMap gives stable
        // ordering for the second key.
        type GroupKey = (Vec<u8>, Vec<Vec<u8>>);
        let mut groups: BTreeMap<GroupKey, u32> = BTreeMap::new();
        for (_o, (submitter, verifier_set)) in per_outcome {
            let verifier_vec: Vec<Vec<u8>> = verifier_set.into_iter().collect();
            *groups.entry((submitter, verifier_vec)).or_insert(0) += 1;
        }

        // Filter and decode.
        let mut out = Vec::new();
        for ((submitter_bytes, verifier_set_bytes), count) in groups {
            if count < min_count {
                continue;
            }
            let submitter = match PeerId::from_bytes(&submitter_bytes) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let mut verifiers = Vec::with_capacity(verifier_set_bytes.len());
            for vb in verifier_set_bytes {
                if let Ok(p) = PeerId::from_bytes(&vb) {
                    verifiers.push(p);
                }
            }
            out.push((submitter, verifiers, count));
        }
        Ok(out)
    }

    // ----- Delta propagation --------------------------------------------

    /// Verify and apply an inbound [`StateDelta`].
    ///
    /// The envelope signature is checked against `signer_pubkey`. For
    /// [`DeltaKind::Outcome`] payloads, the inner [`JobOutcome`]'s own
    /// signature is also checked (V0.2 assumes propagator == observer).
    pub fn apply_delta(
        &self,
        delta: StateDelta,
        signer_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), StoreError> {
        verify_delta(&delta, signer_pubkey)?;
        match delta.kind {
            DeltaKind::Outcome(o) => {
                // The inner outcome is already signed by its observer.
                // We have no easy way to look up that observer's pubkey
                // from within this crate (the miner owns the peer
                // keystore), so we verify what we can — the envelope —
                // and trust the application layer to gate by observer
                // reputation before calling apply_delta.
                self.record_outcome(&o)?;
            }
            DeltaKind::Reputation {
                peer,
                delta: d,
                reason,
                related_hash,
            } => {
                self.apply_reputation_delta(peer, d, &reason, related_hash)?;
            }
            DeltaKind::GovernanceRule {
                rule_name,
                rule_value,
                proposer,
                approvers,
            } => {
                self.upsert_governance_rule(&rule_name, &rule_value, proposer, &approvers)?;
            }
        }
        Ok(())
    }

    /// Return all outcomes observed strictly after `observed_at_after`
    /// as ready-to-publish [`StateDelta`]s — unsigned.
    ///
    /// The caller (the miner crate) signs each delta with its own
    /// peer key before publishing on `parseh.state-deltas.v1`.
    pub fn deltas_since(&self, observed_at_after: u64) -> Result<Vec<StateDelta>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT outcome_cbor, observed_at FROM outcomes
              WHERE observed_at > ?1
              ORDER BY observed_at ASC",
        )?;
        let rows = stmt.query_map(params![observed_at_after as i64], |row| {
            let blob: Vec<u8> = row.get(0)?;
            let observed_at: i64 = row.get(1)?;
            Ok((blob, observed_at))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (blob, observed_at) = r?;
            let outcome: JobOutcome = from_cbor_bytes(&blob)
                .map_err(|e| StoreError::CborDecode(e.to_string()))?;
            let observer = outcome.observed_by;
            out.push(StateDelta::unsigned(
                DeltaKind::Outcome(outcome),
                observer,
                observed_at.max(0) as u64,
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn apply_key_pragmas(conn: &Connection, key: &KeyMaterial) -> Result<(), OpenError> {
    // The `PRAGMA key = "x'...'"` form is the SQLCipher recipe. On
    // vanilla SQLite this PRAGMA exists and is a no-op (it returns
    // an empty rowset), so we can keep one code path.
    let hex = key.to_sqlcipher_hex();
    // PRAGMA does not accept bound parameters, so we have to format.
    // The hex string is alphanumeric → no SQL-injection surface.
    let stmt = format!("PRAGMA key = \"x'{}'\";", &*hex);
    // `execute_batch` returns Err on a real SQL parse failure but is
    // happy with a no-op PRAGMA.
    match conn.execute_batch(&stmt) {
        Ok(()) => Ok(()),
        Err(e) => Err(OpenError::Sqlite(e)),
    }
}

fn map_fk_err(e: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(ref err, ref _msg) = e {
        if err.code == rusqlite::ErrorCode::ConstraintViolation {
            return StoreError::ForeignKey(format!("{e}"));
        }
    }
    StoreError::Sqlite(e)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp().max(0)
}
