//! Forward-only schema migrations.
//!
//! V0.2 starts at [`crate::SCHEMA_VERSION`] = 1. A fresh database is
//! brought up by applying [`crate::schema::SCHEMA_V1`] and recording
//! `(version=1, applied_at=now)` in `schema_version`. Reopening an
//! existing v1 database is a no-op.
//!
//! When V0.3 adds a v2 step, it goes here as a separate function gated
//! on `current_version == 1`. Never edit older steps in place — that
//! would let two peers with different binaries diverge on what "v1"
//! contains.

use rusqlite::{params, Connection, Result as SqlResult};

use crate::{schema::SCHEMA_V1, SCHEMA_VERSION};

/// Apply pending migrations on `conn`.
///
/// Behaviour:
///
/// 1. Read the highest version recorded in `schema_version` (creating
///    the table first if necessary).
/// 2. If that version equals [`crate::SCHEMA_VERSION`], return.
/// 3. Otherwise apply each missing step in order and append a row to
///    `schema_version`.
///
/// All work runs inside a single transaction so a crash mid-migration
/// leaves the DB at the previous version, not somewhere in between.
pub fn run_migrations(conn: &mut Connection) -> SqlResult<u32> {
    let tx = conn.transaction()?;
    // Make sure `schema_version` exists even on a brand-new file.
    // We do this outside the version cascade so that the first read
    // below succeeds.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let current: u32 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u32;

    let now_unix: i64 = chrono::Utc::now().timestamp().max(0);

    if current < 1 {
        // Apply v1 baseline.
        tx.execute_batch(SCHEMA_V1)?;
        tx.execute(
            "INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            params![1i64, now_unix],
        )?;
    }

    // Additive index ensure-step. `idx_outcomes_finalised_at` backs the
    // `/parseh/state-sync/1.0.0` responder query (added 2026-05-15). It
    // is in `SCHEMA_V1` for fresh databases; this `IF NOT EXISTS`
    // statement also brings *existing* v1 on-disk databases up to date
    // without a `SCHEMA_VERSION` bump — an index is a performance
    // addition, not a logical-schema change, so the cross-binary
    // divergence concern that motivates the "never edit old steps"
    // rule does not apply (every binary computes the same query result
    // with or without the index; only speed differs).
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_outcomes_finalised_at \
             ON outcomes(finalised_at);",
    )?;

    // (Future) Insert further migrations here, each gated on the
    // previously-applied version, e.g.:
    //   if current < 2 { tx.execute_batch(SCHEMA_V2_DELTA)?; ... }

    tx.commit()?;
    Ok(SCHEMA_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn fresh_db_lands_at_schema_v1() {
        let mut c = fresh();
        let v = run_migrations(&mut c).unwrap();
        assert_eq!(v, 1);
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn run_migrations_is_idempotent() {
        let mut c = fresh();
        run_migrations(&mut c).unwrap();
        run_migrations(&mut c).unwrap();
        run_migrations(&mut c).unwrap();
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        // Only one row even after three calls.
        assert_eq!(count, 1);
    }

    #[test]
    fn all_five_tables_exist_after_v1() {
        let mut c = fresh();
        run_migrations(&mut c).unwrap();
        for table in [
            "tasks",
            "results",
            "verifications",
            "outcomes",
            "reputation_log",
            "governance_rules",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {table} missing");
        }
    }
}
