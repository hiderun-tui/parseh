//! Polls the GitHub releases API for newer versions of parseh-miner.
//!
//! V0.1 only logs a notice. V0.2 (when releases are Sigstore-signed)
//! will offer to download and restart with a verified replacement.
//!
//! We use `reqwest` with `rustls-tls` so we don't drag OpenSSL into
//! the Windows build.

use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use tracing::{info, warn};

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/hiderun-tui/parseh/releases/latest";

const USER_AGENT: &str = concat!("parseh-miner/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
}

/// Spawn-and-forget: runs forever, checking once on startup and again
/// every 6 hours. Aborts cleanly when the JoinHandle is dropped.
pub async fn run_periodic_check() {
    let mut interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
    loop {
        interval.tick().await;
        if let Err(e) = check_once().await {
            warn!(error = %e, "update check failed (will retry in 6h)");
        }
    }
}

async fn check_once() -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("github returned {}", resp.status());
    }
    let release: GhRelease = resp.json().await?;
    let latest_tag = release.tag_name.trim_start_matches('v');
    let latest = Version::parse(latest_tag)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION").trim_start_matches('v'))
        .unwrap_or_else(|_| Version::new(0, 0, 0));
    if latest > current {
        info!(
            current = %current,
            latest  = %latest,
            url     = %release.html_url,
            "update available — V0.2 will offer automatic install"
        );
    } else {
        tracing::debug!(latest = %latest, "running latest release");
    }
    Ok(())
}
