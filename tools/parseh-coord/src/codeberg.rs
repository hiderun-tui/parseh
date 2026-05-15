//! Real Codeberg connector.
//!
//! Codeberg runs Forgejo (a Gitea fork). Its REST API base is
//! `https://codeberg.org/api/v1` and is close to the GitHub REST API
//! modeled in `github.rs`. The two meaningful differences this connector
//! accounts for:
//!  - Auth header is Forgejo style: `Authorization: token <TOKEN>` (NOT
//!    `Bearer`).
//!  - Forgejo's issue-creation endpoint expects `labels` as an array of
//!    integer label IDs, not names. We do not resolve names to IDs (that
//!    would need an extra round-trip per repo); broadcast to Codeberg
//!    creates issues WITHOUT labels and says so. This is stated honestly
//!    in the README "Limitations".
//!
//! Credentials are read from the environment ONLY (or, indirectly, from
//! `~/.parseh/coord-creds.toml` which `main.rs` loads into the process env
//! before constructing this). Nothing is ever read from the repo.
//!
//!  - `PARSEH_COORD_CODEBERG_TOKEN` — a Forgejo access token with repo
//!    issue read/write scope. REQUIRED for any operation.
//!  - `PARSEH_COORD_CODEBERG_REPO`  — `owner/name`. REQUIRED (no default:
//!    the project's Codeberg mirror does not exist yet, so guessing one
//!    would be dishonest).
//!
//! `poll()` pulls open issues + their comments (REST). `post()` posts an
//! issue comment. `create_issue()` (used by the `broadcast-issues` CLI
//! with `--platform codeberg`) creates a new issue.
//!
//! If the token or repo is absent every method fails with a friendly error
//! that names the exact env var. We never panic. A 404 on the repo is
//! turned into a clear, actionable "repo not found" error.

use crate::connector::Connector;
use crate::store::IngestEvent;
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

const API_BASE: &str = "https://codeberg.org/api/v1";
const USER_AGENT: &str = "parseh-coord/0.1.0-alpha (+https://github.com/hiderun-tui/parseh)";

pub struct CodebergConnector {
    token: Option<String>,
    /// `owner/repo` was required (no default) — these are only set when the
    /// env/creds value was present and well-formed; otherwise `None` so the
    /// missing-repo path produces a friendly error instead of a panic.
    owner: Option<String>,
    repo: Option<String>,
    client: reqwest::blocking::Client,
}

impl CodebergConnector {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("PARSEH_COORD_CODEBERG_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        // No default repo: unlike GitHub, the Codeberg mirror is not a
        // known fixed location. A missing/blank value yields None and a
        // friendly error at call time (never a panic).
        let (owner, repo) = match std::env::var("PARSEH_COORD_CODEBERG_REPO")
            .ok()
            .filter(|r| !r.is_empty())
        {
            Some(full) => match full.split_once('/') {
                Some((o, r)) if !o.is_empty() && !r.is_empty() => {
                    (Some(o.to_string()), Some(r.to_string()))
                }
                _ => {
                    anyhow::bail!(
                        "PARSEH_COORD_CODEBERG_REPO must be 'owner/name', got {full:?}"
                    );
                }
            },
            None => (None, None),
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;
        Ok(CodebergConnector {
            token,
            owner,
            repo,
            client,
        })
    }

    fn token(&self) -> Result<&str> {
        self.token.as_deref().context(
            "Codeberg token missing: set the PARSEH_COORD_CODEBERG_TOKEN environment variable \
             (a Forgejo access token with repo issue read/write scope), or add it to \
             ~/.parseh/coord-creds.toml as `codeberg_token`. \
             parseh-coord never reads credentials from the repository.",
        )
    }

    fn target(&self) -> Result<(&str, &str)> {
        match (self.owner.as_deref(), self.repo.as_deref()) {
            (Some(o), Some(r)) => Ok((o, r)),
            _ => anyhow::bail!(
                "Codeberg repo missing: set the PARSEH_COORD_CODEBERG_REPO environment variable \
                 to 'owner/name' (or add `codeberg_repo` to ~/.parseh/coord-creds.toml). \
                 There is no default — the Codeberg mirror must be created/migrated first."
            ),
        }
    }

    /// Forgejo auth: `Authorization: token <TOKEN>` (NOT Bearer).
    fn auth_header(token: &str) -> String {
        format!("token {token}")
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
        if status.as_u16() == 404 {
            let (o, r) = self.target()?;
            anyhow::bail!(
                "Codeberg GET {url} -> HTTP 404. The repo {o}/{r} was not found on \
                 Codeberg — create/migrate it there first (or fix \
                 PARSEH_COORD_CODEBERG_REPO). parseh-coord does not create the \
                 repository for you."
            );
        }
        if !status.is_success() {
            anyhow::bail!("Codeberg GET {url} -> HTTP {status}: {}", truncate(&text, 300));
        }
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
    }

    fn poll_open_issues(&self) -> Result<Vec<IngestEvent>> {
        let (owner, repo) = self.target()?;
        // type=issues excludes pull requests on Forgejo.
        let url = format!(
            "{API_BASE}/repos/{owner}/{repo}/issues?state=open&type=issues&limit=50"
        );
        let arr = self.rest_get(&url)?;
        let mut out = Vec::new();
        for item in arr.as_array().cloned().unwrap_or_default() {
            // Belt-and-braces: skip anything that is actually a PR.
            if item.get("pull_request").map(|p| !p.is_null()).unwrap_or(false) {
                continue;
            }
            let number = item.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
            out.push(IngestEvent {
                platform: "codeberg".into(),
                kind: "issue".into(),
                thread_ref: number.to_string(),
                author: str_at(&item, &["user", "login"]),
                body: item
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|t| {
                        let b = item.get("body").and_then(|b| b.as_str()).unwrap_or("");
                        format!("{t}\n\n{b}")
                    })
                    .unwrap_or_default(),
                url: item
                    .get("html_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: parse_ts(item.get("created_at").and_then(|c| c.as_str())),
            });
        }
        Ok(out)
    }

    fn poll_issue_comments(&self, issue_number: i64) -> Result<Vec<IngestEvent>> {
        let (owner, repo) = self.target()?;
        let url = format!(
            "{API_BASE}/repos/{owner}/{repo}/issues/{issue_number}/comments"
        );
        let arr = self.rest_get(&url)?;
        let mut out = Vec::new();
        for c in arr.as_array().cloned().unwrap_or_default() {
            // Stable thread_ref = the parent issue number (matches the
            // github.rs convention so `post()` replies land on the issue).
            out.push(IngestEvent {
                platform: "codeberg".into(),
                kind: "issue_comment".into(),
                thread_ref: issue_number.to_string(),
                author: str_at(&c, &["user", "login"]),
                body: c.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                url: c
                    .get("html_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: parse_ts(c.get("created_at").and_then(|x| x.as_str())),
            });
        }
        Ok(out)
    }

    /// Create a new issue. Used by the `broadcast-issues --platform
    /// codeberg` CLI path. Returns the html_url of the created issue.
    ///
    /// NOTE: Forgejo's create-issue endpoint expects `labels` as integer
    /// label IDs, not names. We do not resolve names → IDs (that needs an
    /// extra round-trip per repo and label set). Issues are therefore
    /// created on Codeberg WITHOUT labels; `labels` is accepted for a
    /// uniform call signature with the GitHub connector but is not sent.
    /// This is stated in the README "Limitations".
    pub fn create_issue(&self, title: &str, body: &str, _labels: &[String]) -> Result<String> {
        let token = self.token()?;
        let (owner, repo) = self.target()?;
        let url = format!("{API_BASE}/repos/{owner}/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
        });
        let resp = self
            .client
            .post(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json")
            .header("Authorization", Self::auth_header(token))
            .json(&payload)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.as_u16() == 404 {
            anyhow::bail!(
                "Codeberg create issue -> HTTP 404. The repo {owner}/{repo} was not \
                 found on Codeberg — create/migrate it there first."
            );
        }
        if !status.is_success() {
            anyhow::bail!(
                "Codeberg create issue -> HTTP {status}: {}",
                truncate(&text, 400)
            );
        }
        let v: Value = serde_json::from_str(&text).context("parsing created-issue JSON")?;
        Ok(v.get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("(no url returned)")
            .to_string())
    }
}

impl Connector for CodebergConnector {
    fn platform(&self) -> &str {
        "codeberg"
    }

    fn poll(&self) -> Result<Vec<IngestEvent>> {
        // Fail early + friendly if token or repo is missing.
        self.token()?;
        self.target()?;
        let mut all = Vec::new();
        let issues = self.poll_open_issues()?;
        // Collect comment-poll targets first (issues vector is moved into
        // `all`).
        let numbers: Vec<i64> = issues
            .iter()
            .filter_map(|e| e.thread_ref.parse::<i64>().ok())
            .collect();
        all.extend(issues);
        for n in numbers {
            all.extend(self.poll_issue_comments(n)?);
        }
        Ok(all)
    }

    fn post(&self, thread_ref: &str, body: &str) -> Result<String> {
        let token = self.token()?;
        let (owner, repo) = self.target()?;
        let index: i64 = thread_ref.parse().with_context(|| {
            format!(
                "codeberg thread_ref must be an issue number, got {thread_ref:?}"
            )
        })?;
        let url = format!(
            "{API_BASE}/repos/{owner}/{repo}/issues/{index}/comments"
        );
        let resp = self
            .client
            .post(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json")
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.as_u16() == 404 {
            anyhow::bail!(
                "Codeberg post comment -> HTTP 404. The repo {owner}/{repo} or issue \
                 #{index} was not found on Codeberg."
            );
        }
        if !status.is_success() {
            anyhow::bail!(
                "Codeberg post comment -> HTTP {status}: {}",
                truncate(&text, 400)
            );
        }
        let v: Value = serde_json::from_str(&text).context("parsing comment JSON")?;
        Ok(v.get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("(no url returned)")
            .to_string())
    }
}

fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(next) => cur = next,
            None => return "(unknown)".to_string(),
        }
    }
    cur.as_str().unwrap_or("(unknown)").to_string()
}

/// Parse an RFC3339 timestamp into unix seconds without pulling chrono.
/// Best-effort: returns 0 on any parse failure. Forgejo returns
/// `created_at` in the same `2026-05-15T16:29:00Z` shape GitHub uses, so
/// this is the identical civil-days algorithm as `github.rs`.
fn parse_ts(s: Option<&str>) -> i64 {
    let s = match s {
        Some(s) => s,
        None => return 0,
    };
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
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

    fn with_token_and_repo() -> CodebergConnector {
        CodebergConnector {
            token: Some("fake".into()),
            owner: Some("hiderun-tui".into()),
            repo: Some("parseh".into()),
            client: reqwest::blocking::Client::new(),
        }
    }

    #[test]
    fn forgejo_auth_header_is_token_not_bearer() {
        // The single most important Forgejo difference vs GitHub.
        assert_eq!(CodebergConnector::auth_header("abc123"), "token abc123");
        assert!(!CodebergConnector::auth_header("abc123").starts_with("Bearer"));
    }

    #[test]
    fn parse_ts_matches_github_shape() {
        let a = parse_ts(Some("2026-05-15T00:00:00Z"));
        let b = parse_ts(Some("2026-05-15T00:00:01Z"));
        let c = parse_ts(Some("2026-05-16T00:00:00Z"));
        assert!(a > 1_700_000_000, "got {a}");
        assert_eq!(b - a, 1);
        assert_eq!(c - a, 86400);
        assert_eq!(parse_ts(None), 0);
        assert_eq!(parse_ts(Some("garbage")), 0);
    }

    #[test]
    fn str_at_handles_missing_path() {
        let v = serde_json::json!({"user": {"login": "alice"}});
        assert_eq!(str_at(&v, &["user", "login"]), "alice");
        assert_eq!(str_at(&v, &["user", "nope"]), "(unknown)");
        assert_eq!(str_at(&v, &["missing"]), "(unknown)");
    }

    #[test]
    fn missing_token_is_friendly_not_panic() {
        let cc = CodebergConnector {
            token: None,
            owner: Some("hiderun-tui".into()),
            repo: Some("parseh".into()),
            client: reqwest::blocking::Client::new(),
        };
        let pe = cc.poll().unwrap_err().to_string();
        assert!(pe.contains("PARSEH_COORD_CODEBERG_TOKEN"), "got: {pe}");
        let se = cc.post("1", "x").unwrap_err().to_string();
        assert!(se.contains("PARSEH_COORD_CODEBERG_TOKEN"), "got: {se}");
        let ce = cc.create_issue("t", "b", &[]).unwrap_err().to_string();
        assert!(ce.contains("PARSEH_COORD_CODEBERG_TOKEN"), "got: {ce}");
    }

    #[test]
    fn missing_repo_is_friendly_not_panic() {
        // Token present, repo absent: must name the repo env var, not panic.
        let cc = CodebergConnector {
            token: Some("fake".into()),
            owner: None,
            repo: None,
            client: reqwest::blocking::Client::new(),
        };
        let pe = cc.poll().unwrap_err().to_string();
        assert!(pe.contains("PARSEH_COORD_CODEBERG_REPO"), "got: {pe}");
        let se = cc.post("1", "x").unwrap_err().to_string();
        assert!(se.contains("PARSEH_COORD_CODEBERG_REPO"), "got: {se}");
        let ce = cc.create_issue("t", "b", &[]).unwrap_err().to_string();
        assert!(ce.contains("PARSEH_COORD_CODEBERG_REPO"), "got: {ce}");
    }

    #[test]
    fn post_rejects_non_numeric_thread_ref_before_network() {
        let cc = with_token_and_repo();
        let e = cc.post("discussion:7", "hi").unwrap_err().to_string();
        assert!(e.contains("issue number"), "got: {e}");
    }

    // --- JSON → IngestEvent normalisation (offline; no network) ---

    /// The normalisation logic lives inside `poll_open_issues` /
    /// `poll_issue_comments` around a network call, so we mirror exactly
    /// that mapping here against representative Forgejo JSON. If the
    /// mapping in the connector changes, these intentionally must change
    /// too — they document the wire contract.
    fn issue_to_event(item: &Value) -> IngestEvent {
        let number = item.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
        IngestEvent {
            platform: "codeberg".into(),
            kind: "issue".into(),
            thread_ref: number.to_string(),
            author: str_at(item, &["user", "login"]),
            body: item
                .get("title")
                .and_then(|t| t.as_str())
                .map(|t| {
                    let b = item.get("body").and_then(|b| b.as_str()).unwrap_or("");
                    format!("{t}\n\n{b}")
                })
                .unwrap_or_default(),
            url: item
                .get("html_url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: parse_ts(item.get("created_at").and_then(|c| c.as_str())),
        }
    }

    #[test]
    fn issue_json_normalises_to_ingest_event() {
        let item = serde_json::json!({
            "number": 42,
            "title": "Build fails on musl",
            "body": "steps to reproduce …",
            "html_url": "https://codeberg.org/hiderun-tui/parseh/issues/42",
            "created_at": "2026-05-15T16:29:00Z",
            "user": { "login": "contributor1" }
        });
        let ev = issue_to_event(&item);
        assert_eq!(ev.platform, "codeberg");
        assert_eq!(ev.kind, "issue");
        assert_eq!(ev.thread_ref, "42");
        assert_eq!(ev.author, "contributor1");
        assert_eq!(ev.body, "Build fails on musl\n\nsteps to reproduce …");
        assert_eq!(ev.url, "https://codeberg.org/hiderun-tui/parseh/issues/42");
        assert!(ev.created_at > 1_700_000_000, "got {}", ev.created_at);
    }

    #[test]
    fn issue_thread_ref_is_deterministic_from_number() {
        let a = issue_to_event(&serde_json::json!({"number": 7, "title": "x"}));
        let b = issue_to_event(&serde_json::json!({"number": 7, "title": "y"}));
        // Same issue number → identical thread_ref regardless of other
        // fields, so dedupe + reply targeting are stable.
        assert_eq!(a.thread_ref, b.thread_ref);
        assert_eq!(a.thread_ref, "7");
    }

    #[test]
    fn comment_thread_ref_points_at_parent_issue() {
        // A comment on issue #42 must carry thread_ref "42" so a drafted
        // reply lands on that issue (matches the github.rs convention).
        let issue_number = 42i64;
        let c = serde_json::json!({
            "id": 9001,
            "body": "thanks, will look",
            "html_url": "https://codeberg.org/hiderun-tui/parseh/issues/42#issuecomment-9001",
            "created_at": "2026-05-15T17:00:00Z",
            "user": { "login": "maintainer" }
        });
        let ev = IngestEvent {
            platform: "codeberg".into(),
            kind: "issue_comment".into(),
            thread_ref: issue_number.to_string(),
            author: str_at(&c, &["user", "login"]),
            body: c.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
            url: c.get("html_url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
            created_at: parse_ts(c.get("created_at").and_then(|x| x.as_str())),
        };
        assert_eq!(ev.thread_ref, "42");
        assert_eq!(ev.kind, "issue_comment");
        assert_eq!(ev.author, "maintainer");
        assert_eq!(ev.body, "thanks, will look");
    }
}
