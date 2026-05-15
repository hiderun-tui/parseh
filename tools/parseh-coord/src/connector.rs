//! The `Connector` trait + the registry that fans out across platforms.
//!
//! Honest scope (conservative-semantics rule applies):
//!  - GitHub: REAL, fully implemented (see `github.rs`).
//!  - Nostr: REAL (see `nostr.rs`). Posting is best-effort across public,
//!    unmoderated relays — not guaranteed, not anonymous, not
//!    "uncensorable". Delivery can be dropped/refused by a relay; failures
//!    are surfaced honestly, never faked.
//!  - Matrix: STUB — trait impl that `bail!`s. Not faked.

use crate::store::IngestEvent;
use anyhow::Result;

/// A platform connector. `poll` is read-only; `post` writes (and is only
/// ever called from the `send` CLI path after an explicit `approve`).
pub trait Connector {
    fn platform(&self) -> &str;
    fn poll(&self) -> Result<Vec<IngestEvent>>;
    /// Post `body` to `thread_ref`. Returns the URL of the created item.
    fn post(&self, thread_ref: &str, body: &str) -> Result<String>;
}

/// Matrix connector — STUB. The trait is wired so a real impl is purely
/// additive later (V0.x). We do not fake poll/post results.
pub struct MatrixConnector;

impl Connector for MatrixConnector {
    fn platform(&self) -> &str {
        "matrix"
    }
    fn poll(&self) -> Result<Vec<IngestEvent>> {
        anyhow::bail!(
            "matrix connector not yet wired — V0.x. \
             (Stubbed deliberately; no fake data is returned.)"
        )
    }
    fn post(&self, _thread_ref: &str, _body: &str) -> Result<String> {
        anyhow::bail!("matrix connector not yet wired — V0.x")
    }
}

// The Nostr connector is REAL and lives in `nostr.rs` (it needs the
// nostr-sdk async stack wrapped in a private blocking bridge). It is
// constructed in `main.rs` exactly like the GitHub one.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fully in-memory connector for trait-level tests. Never hits a
    /// network. Records every post.
    pub struct MockConnector {
        name: String,
        events: Vec<IngestEvent>,
        pub posted: RefCell<Vec<(String, String)>>,
        fail_post: bool,
    }

    impl MockConnector {
        fn new(name: &str, events: Vec<IngestEvent>, fail_post: bool) -> Self {
            MockConnector {
                name: name.into(),
                events,
                posted: RefCell::new(Vec::new()),
                fail_post,
            }
        }
    }

    impl Connector for MockConnector {
        fn platform(&self) -> &str {
            &self.name
        }
        fn poll(&self) -> Result<Vec<IngestEvent>> {
            Ok(self.events.clone())
        }
        fn post(&self, thread_ref: &str, body: &str) -> Result<String> {
            if self.fail_post {
                anyhow::bail!("mock post failure");
            }
            self.posted
                .borrow_mut()
                .push((thread_ref.to_string(), body.to_string()));
            Ok(format!("mock://{}/{thread_ref}", self.name))
        }
    }

    fn sample(author: &str) -> IngestEvent {
        IngestEvent {
            platform: "mock".into(),
            kind: "issue".into(),
            thread_ref: "1".into(),
            author: author.into(),
            body: "b".into(),
            url: "u".into(),
            created_at: 1,
        }
    }

    #[test]
    fn mock_poll_returns_seeded_events() {
        let c = MockConnector::new("mock", vec![sample("a"), sample("b")], false);
        let evs = c.poll().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(c.platform(), "mock");
    }

    #[test]
    fn mock_post_records_and_returns_url() {
        let c = MockConnector::new("mock", vec![], false);
        let url = c.post("7", "hi there").unwrap();
        assert_eq!(url, "mock://mock/7");
        assert_eq!(c.posted.borrow().len(), 1);
        assert_eq!(c.posted.borrow()[0], ("7".to_string(), "hi there".to_string()));
    }

    #[test]
    fn mock_post_failure_propagates() {
        let c = MockConnector::new("mock", vec![], true);
        assert!(c.post("1", "x").is_err());
    }

    #[test]
    fn matrix_stub_bails_clearly() {
        let m = MatrixConnector;
        assert_eq!(m.platform(), "matrix");
        let e = m.poll().unwrap_err().to_string();
        assert!(e.contains("matrix connector not yet wired"), "got: {e}");
        assert!(m.post("1", "x").is_err());
    }
    // Nostr is no longer a stub — its real behaviour is tested in nostr.rs.
}
