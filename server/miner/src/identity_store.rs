//! Persistent libp2p identity. Stored as 32 raw ed25519 secret bytes
//! at `<config_dir>/identity.ed25519` with 0600 permissions on Unix.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use libp2p::identity::{ed25519, Keypair};
use rand::RngCore;

const IDENTITY_FILE: &str = "identity.ed25519";

/// Load the identity from `config_dir/identity.ed25519`, or create a new
/// one and persist it. Returns `(keypair, was_created)`.
pub fn load_or_generate(config_dir: &Path) -> Result<(Keypair, bool)> {
    let path = config_dir.join(IDENTITY_FILE);
    if path.exists() {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "identity file at {} has {} bytes; expected 32. Delete it to regenerate.",
                path.display(),
                bytes.len()
            );
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes);
        let secret = ed25519::SecretKey::try_from_bytes(&mut buf)
            .context("invalid ed25519 secret in identity file")?;
        Ok((Keypair::from(ed25519::Keypair::from(secret)), false))
    } else {
        fs::create_dir_all(config_dir)
            .with_context(|| format!("create dir {}", config_dir.display()))?;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret_bytes);
        write_secret_file(&path, &secret_bytes)
            .with_context(|| format!("write {}", path.display()))?;
        let secret = ed25519::SecretKey::try_from_bytes(&mut secret_bytes)?;
        Ok((Keypair::from(ed25519::Keypair::from(secret)), true))
    }
}

/// Writes the secret with restrictive perms where the OS supports it.
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
        // On Windows the file inherits the user's profile ACLs, which is
        // typically only-owner-readable. V0.2 will set ACLs explicitly.
        Ok(())
    }
}
