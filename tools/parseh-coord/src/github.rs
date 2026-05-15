//! Real GitHub connector.
//!
//! Credentials are read from the environment ONLY (or, indirectly, from
//! `~/.parseh/coord-creds.toml` which `main.rs` loads into the process env
//! before constructing this). Nothing is ever read from the repo.
//!
//!  - `PARSEH_COORD_GITHUB_TOKEN` — a PAT (classic or fine-grained) with
//!    `repo` (or `public_repo` + `discussions`) scope. REQUIRED.
//!  - `PARSEH_COORD_GITHUB_REPO`  — `owner/name`, default `hiderun-tui/parseh`.
//!
//! `poll()` pulls:
//!  - open issues (REST, excludes PRs)
//!  - issue comments across the repo (REST)
//!  - discussion comments (GraphQL — Discussions are GraphQL-only)
//!
//! `post()` posts an issue comment. `create_issue()` (used by the
//! `broadcast-issues` CLI) creates a new issue.
//!
//! If the token is absent every method fails with a friendly error that
//! names the env var. We never panic.

use crate::connector::Connector;
use crate::store::IngestEvent;
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

const DEFAULT_REPO: &str = "hiderun-tui/parseh";
const USER_AGENT: &str = "parseh-coord/0.1.0-alpha (+https://github.com/hiderun-tui/parseh)";

pub struct GithubConnector {
    token: Option<String>,
    owner: String,
    repo: String,
    client: reqwest::blocking::Client,
}

impl GithubConnector {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("PARSEH_COORD_GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
        let repo_full =
            std::env::var("PARSEH_COORD_GITHUB_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
        let (owner, repo) = repo_full
            .split_once('/')
            .with_context(|| format!("PARSEH_COORD_GITHUB_REPO must be 'owner/name', got {repo_full:?}"))?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;
        Ok(GithubConnector {
            token,
            owner: owner.to_string(),
            repo: repo.to_string(),
            client,
        })
    }

    fn token(&self) -> Result<&str> {
        self.token.as_deref().context(
            "GitHub token missing: set the PARSEH_COORD_GITHUB_TOKEN environment variable \
             (a PAT with 'repo' scope), or add it to ~/.parseh/coord-creds.toml. \
             parseh-coord never reads credentials from the repository.",
        )
    }

    fn rest_get(&self, url: &str) -> Result<Value> {
        let token = self.token()?;
        let resp = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GitHub GET {url} -> HTTP {status}: {}", truncate(&text, 300));
        }
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let token = self.token()?;
        let body = serde_json::json!({ "query": query, "variables": variables });
        let resp = self
            .client
            .post("https://api.github.com/graphql")
            .header("User-Agent", USER_AGENT)
            .bearer_auth(token)
            .json(&body)
            .send()
            .context("POST https://api.github.com/graphql")?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GitHub GraphQL -> HTTP {status}: {}", truncate(&text, 300));
        }
        let v: Value = serde_json::from_str(&text).context("parsing GraphQL JSON")?;
        if let Some(errs) = v.get("errors") {
            anyhow::bail!("GitHub GraphQL errors: {errs}");
        }
        Ok(v)
    }

    fn poll_open_issues(&self) -> Result<Vec<IngestEvent>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues?state=open&per_page=50&sort=updated",
            self.owner, self.repo
        );
        let arr = self.rest_get(&url)?;
        let mut out = Vec::new();
        for item in arr.as_array().cloned().unwrap_or_default() {
            // The issues endpoint also returns PRs; skip those.
            if item.get("pull_request").is_some() {
                continue;
            }
            let number = item.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
            out.push(IngestEvent {
                platform: "github".into(),
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

    fn poll_issue_comments(&self) -> Result<Vec<IngestEvent>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/comments?per_page=100&sort=created&direction=desc",
            self.owner, self.repo
        );
        let arr = self.rest_get(&url)?;
        let mut out = Vec::new();
        for c in arr.as_array().cloned().unwrap_or_default() {
            // issue_url ends in /issues/<n>; derive the thread ref.
            let issue_url = c.get("issue_url").and_then(|u| u.as_str()).unwrap_or("");
            let thread_ref = issue_url
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            out.push(IngestEvent {
                platform: "github".into(),
                kind: "issue_comment".into(),
                thread_ref,
                author: str_at(&c, &["user", "login"]),
                body: c.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                url: c.get("html_url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                created_at: parse_ts(c.get("created_at").and_then(|x| x.as_str())),
            });
        }
        Ok(out)
    }

    fn poll_discussion_comments(&self) -> Result<Vec<IngestEvent>> {
        let query = r#"
            query($owner:String!, $repo:String!) {
              repository(owner:$owner, name:$repo) {
                discussions(first:25, orderBy:{field:UPDATED_AT, direction:DESC}) {
                  nodes {
                    number
                    title
                    url
                    comments(first:25) {
                      nodes {
                        author { login }
                        bodyText
                        url
                        createdAt
                      }
                    }
                  }
                }
              }
            }
        "#;
        let v = match self.graphql(
            query,
            serde_json::json!({ "owner": self.owner, "repo": self.repo }),
        ) {
            Ok(v) => v,
            Err(e) => {
                // Discussions may be disabled on the repo — that is not a
                // hard failure for ingest. Surface it but don't abort.
                eprintln!("note: discussion-comment poll skipped: {e}");
                return Ok(Vec::new());
            }
        };
        let mut out = Vec::new();
        let discussions = v
            .pointer("/data/repository/discussions/nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        for d in discussions {
            let number = d.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
            let comments = d
                .pointer("/comments/nodes")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            for c in comments {
                out.push(IngestEvent {
                    platform: "github".into(),
                    kind: "discussion_comment".into(),
                    thread_ref: format!("discussion:{number}"),
                    author: c
                        .pointer("/author/login")
                        .and_then(|x| x.as_str())
                        .unwrap_or("(unknown)")
                        .to_string(),
                    body: c.get("bodyText").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    url: c.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    created_at: parse_ts(c.get("createdAt").and_then(|x| x.as_str())),
                });
            }
        }
        Ok(out)
    }

    /// Create a new issue. Used by the `broadcast-issues` CLI command.
    /// Returns the html_url of the created issue.
    pub fn create_issue(&self, title: &str, body: &str, labels: &[String]) -> Result<String> {
        let token = self.token()?;
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues",
            self.owner, self.repo
        );
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let resp = self
            .client
            .post(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .json(&payload)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("create issue -> HTTP {status}: {}", truncate(&text, 400));
        }
        let v: Value = serde_json::from_str(&text).context("parsing created-issue JSON")?;
        Ok(v.get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("(no url returned)")
            .to_string())
    }
}

impl Connector for GithubConnector {
    fn platform(&self) -> &str {
        "github"
    }

    fn poll(&self) -> Result<Vec<IngestEvent>> {
        // Fail early + friendly if no token.
        self.token()?;
        let mut all = Vec::new();
        all.extend(self.poll_open_issues()?);
        all.extend(self.poll_issue_comments()?);
        all.extend(self.poll_discussion_comments()?);
        Ok(all)
    }

    fn post(&self, thread_ref: &str, body: &str) -> Result<String> {
        let token = self.token()?;
        if thread_ref.starts_with("discussion:") {
            anyhow::bail!(
                "posting to discussion threads is not implemented yet (thread_ref={thread_ref}); \
                 reply to issue threads or open an issue instead"
            );
        }
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            self.owner, self.repo, thread_ref
        );
        let resp = self
            .client
            .post(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("post comment -> HTTP {status}: {}", truncate(&text, 400));
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
/// Best-effort: returns 0 on any parse failure (sorting still works
/// because GitHub returns items already ordered).
fn parse_ts(s: Option<&str>) -> i64 {
    let s = match s {
        Some(s) => s,
        None => return 0,
    };
    // Format: 2026-05-15T16:29:00Z
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return 0;
    }
    let g = |a: usize, b: usize| s[a..b].parse::<i64>().unwrap_or(0);
    let (y, mo, d) = (g(0, 4), g(5, 7), g(8, 10));
    let (h, mi, se) = (g(11, 13), g(14, 16), g(17, 19));
    // Days from civil (Howard Hinnant's algorithm).
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

    #[test]
    fn parse_ts_known_value() {
        // 2026-05-15T00:00:00Z — sanity: must be > the 2024 epoch range and
        // monotonic with a later timestamp.
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
    fn missing_token_is_friendly_not_panic() {
        // Construct with no token; poll/post must Err, naming the env var.
        let gc = GithubConnector {
            token: None,
            owner: "hiderun-tui".into(),
            repo: "parseh".into(),
            client: reqwest::blocking::Client::new(),
        };
        let pe = gc.poll().unwrap_err().to_string();
        assert!(pe.contains("PARSEH_COORD_GITHUB_TOKEN"), "got: {pe}");
        let se = gc.post("1", "x").unwrap_err().to_string();
        assert!(se.contains("PARSEH_COORD_GITHUB_TOKEN"), "got: {se}");
        let ce = gc.create_issue("t", "b", &[]).unwrap_err().to_string();
        assert!(ce.contains("PARSEH_COORD_GITHUB_TOKEN"), "got: {ce}");
    }

    #[test]
    fn discussion_thread_post_is_rejected_clearly() {
        // Even with a (fake) token, posting to a discussion ref is refused
        // before any network call.
        let gc = GithubConnector {
            token: Some("fake".into()),
            owner: "o".into(),
            repo: "r".into(),
            client: reqwest::blocking::Client::new(),
        };
        let e = gc.post("discussion:7", "hi").unwrap_err().to_string();
        assert!(e.contains("discussion"), "got: {e}");
    }

    #[test]
    fn str_at_handles_missing_path() {
        let v = serde_json::json!({"user": {"login": "alice"}});
        assert_eq!(str_at(&v, &["user", "login"]), "alice");
        assert_eq!(str_at(&v, &["user", "nope"]), "(unknown)");
        assert_eq!(str_at(&v, &["missing"]), "(unknown)");
    }
}
