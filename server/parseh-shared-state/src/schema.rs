//! SQL DDL for the V0.2 shared-state schema.
//!
//! Tables (all created in [`SCHEMA_V1`]):
//!
//! - `tasks` — every [`parseh_task::JobSpec`] observed.
//! - `results` — every [`parseh_task::JobResult`] observed.
//! - `verifications` — every [`parseh_task::JobVerification`] observed.
//! - `outcomes` — every signed [`parseh_task::JobOutcome`] observed.
//! - `reputation_log` — append-only deltas per peer.
//! - `governance_rules` — amendments that have passed §11 process.
//! - `schema_version` — tracks the on-disk migration level.
//!
//! See the project notes §5 and
//! the project notes §3.3.

/// The V0.2 baseline schema. Idempotent — `IF NOT EXISTS` everywhere.
///
/// Applied by [`crate::migrations::run_migrations`] on every open. Once
/// a migration to v2 lands, this constant freezes and v1 becomes the
/// "version 1" step inside [`crate::migrations`]; do **not** edit
/// `SCHEMA_V1` in-place.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    spec_hash    BLOB PRIMARY KEY,
    submitter    BLOB NOT NULL,
    kind         TEXT NOT NULL,
    sensitive    INTEGER NOT NULL,
    submitted_at INTEGER NOT NULL,
    spec_cbor    BLOB NOT NULL,
    observed_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_submitter    ON tasks(submitter);
CREATE INDEX IF NOT EXISTS idx_tasks_submitted_at ON tasks(submitted_at);

CREATE TABLE IF NOT EXISTS results (
    result_hash BLOB PRIMARY KEY,
    spec_hash   BLOB NOT NULL REFERENCES tasks(spec_hash) ON DELETE CASCADE,
    executor    BLOB NOT NULL,
    executed_at INTEGER NOT NULL,
    result_cbor BLOB NOT NULL,
    observed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_results_spec     ON results(spec_hash);
CREATE INDEX IF NOT EXISTS idx_results_executor ON results(executor);

CREATE TABLE IF NOT EXISTS verifications (
    verification_hash BLOB PRIMARY KEY,
    result_hash       BLOB NOT NULL REFERENCES results(result_hash) ON DELETE CASCADE,
    verifier          BLOB NOT NULL,
    verdict           TEXT NOT NULL,
    method            TEXT NOT NULL,
    verified_at       INTEGER NOT NULL,
    verification_cbor BLOB NOT NULL,
    observed_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_verifications_result   ON verifications(result_hash);
CREATE INDEX IF NOT EXISTS idx_verifications_verifier ON verifications(verifier);

CREATE TABLE IF NOT EXISTS outcomes (
    outcome_hash BLOB PRIMARY KEY,
    spec_hash    BLOB NOT NULL REFERENCES tasks(spec_hash) ON DELETE CASCADE,
    result_hash  BLOB NOT NULL REFERENCES results(result_hash) ON DELETE CASCADE,
    verdict      TEXT NOT NULL,
    finalised_at INTEGER NOT NULL,
    observed_by  BLOB NOT NULL,
    outcome_cbor BLOB NOT NULL,
    observed_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_outcomes_spec ON outcomes(spec_hash);
-- Backs `SharedState::outcomes_since` (the `/parseh/state-sync/1.0.0`
-- responder query): range-scan `finalised_at >= ?` ordered DESC.
CREATE INDEX IF NOT EXISTS idx_outcomes_finalised_at ON outcomes(finalised_at);

CREATE TABLE IF NOT EXISTS reputation_log (
    entry_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    peer         BLOB NOT NULL,
    delta        INTEGER NOT NULL,
    reason       TEXT NOT NULL,
    related_hash BLOB,
    applied_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reputation_peer       ON reputation_log(peer);
CREATE INDEX IF NOT EXISTS idx_reputation_applied_at ON reputation_log(applied_at);

CREATE TABLE IF NOT EXISTS governance_rules (
    rule_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name     TEXT NOT NULL UNIQUE,
    rule_value    TEXT NOT NULL,
    proposed_at   INTEGER NOT NULL,
    activated_at  INTEGER NOT NULL,
    proposer      BLOB NOT NULL,
    approvers     BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_governance_name ON governance_rules(rule_name);

CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);
"#;
