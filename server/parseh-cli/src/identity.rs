//! Persistent ed25519 identity, miner-compatible.
//!
//! The miner stores 32 raw ed25519 secret bytes at
//! `<config_dir>/identity.ed25519` with mode 0600 on Unix. The CLI reads
//! the same file. If it does not exist we generate one — `parseh
//! whoami` is the documented bootstrap path.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use libp2p::identity::{ed25519, Keypair};
use libp2p::PeerId;
use rand::RngCore;

/// Load or generate the libp2p identity at `path`.
///
/// Returns `(keypair, peer_id, was_created)`. The `was_created` flag is
/// surfaced so commands like `whoami` can tell the user "generated a
/// fresh key — back this file up".
pub fn load_or_generate(path: &Path) -> Result<(Keypair, PeerId, bool)> {
    if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("read identity {}", path.display()))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "identity file at {} has {} bytes; expected 32 (miner format). Delete it to regenerate.",
                path.display(),
                bytes.len()
            );
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes);
        let secret = ed25519::SecretKey::try_from_bytes(&mut buf)
            .context("invalid ed25519 secret in identity file")?;
        let kp = Keypair::from(ed25519::Keypair::from(secret));
        let peer = PeerId::from(kp.public());
        Ok((kp, peer, false))
    } else {
        let parent = path
            .parent()
            .context("identity path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create identity parent dir {}", parent.display()))?;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret_bytes);
        write_secret_file(path, &secret_bytes)
            .with_context(|| format!("write identity {}", path.display()))?;
        let secret = ed25519::SecretKey::try_from_bytes(&mut secret_bytes)?;
        let kp = Keypair::from(ed25519::Keypair::from(secret));
        let peer = PeerId::from(kp.public());
        Ok((kp, peer, true))
    }
}

/// Derive an `ed25519_dalek::SigningKey` from a libp2p ed25519 keypair.
///
/// `parseh-task::JobSpec::new_signed` expects a dalek `SigningKey`, but
/// the miner stores a libp2p ed25519 secret. The two formats are
/// byte-identical at the secret level, so we re-encode through the raw
/// 32-byte secret. This keeps `parseh whoami` and `parseh submit`
/// signing the same logical key.
pub fn signing_key_from_libp2p(kp: &Keypair) -> Result<ed25519_dalek::SigningKey> {
    let ed = kp
        .clone()
        .try_into_ed25519()
        .map_err(|e| anyhow::anyhow!("keypair is not ed25519: {e}"))?;
    let secret_bytes = ed.secret();
    let raw: [u8; 32] = secret_bytes
        .as_ref()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32-byte ed25519 secret"))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&raw))
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    }
}
