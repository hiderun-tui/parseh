//! Persistent libp2p identity for `parseh-relay`.
//!
//! Stored as 32 raw ed25519 secret bytes at
//! `<config_dir>/relay-identity.ed25519` with 0600 permissions on Unix.
//!
//! Mirrors the pattern used by `parseh-miner` (`server/miner/src/identity_store.rs`)
//! but keeps the relay's key file under a distinct name so the two binaries
//! can share a config directory without colliding.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use libp2p::identity::{ed25519, Keypair};
use rand::RngCore;

const IDENTITY_FILE: &str = "relay-identity.ed25519";

/// Load the identity from `config_dir/relay-identity.ed25519`, or create a new
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Tiny scoped temp dir so we don't pull `tempfile` into the relay crate
    /// just for one test. Cleans itself up on drop.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = env::temp_dir();
            let nonce = format!(
                "parseh-relay-id-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            p.push(nonce);
            fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Save → load must yield the exact same 32 ed25519 secret bytes,
    /// i.e. the same on-disk identity round-trips identically.
    #[test]
    fn save_then_load_returns_same_key_bytes() {
        let dir = TempDir::new("roundtrip");

        // First call creates the file and returns a fresh keypair.
        let (kp1, created1) = load_or_generate(dir.path()).expect("first load_or_generate");
        assert!(created1, "first call should create the identity file");

        let key_path = dir.path().join(IDENTITY_FILE);
        assert!(key_path.exists(), "identity file must be on disk after first call");

        // The on-disk format is 32 raw secret bytes — confirm that invariant.
        let on_disk = fs::read(&key_path).expect("read identity file");
        assert_eq!(on_disk.len(), 32, "identity file must be exactly 32 bytes");

        // Second call must load the same key, not regenerate.
        let (kp2, created2) = load_or_generate(dir.path()).expect("second load_or_generate");
        assert!(!created2, "second call must load, not create");

        // The PeerId is derived deterministically from the ed25519 secret,
        // so a matching PeerId across two `load_or_generate` calls is the
        // tightest possible round-trip assertion without re-implementing
        // the libp2p key-derivation here.
        let peer1 = libp2p::PeerId::from(kp1.public());
        let peer2 = libp2p::PeerId::from(kp2.public());
        assert_eq!(peer1, peer2, "derived PeerId must be identical across save/load");

        // And the secret bytes derived from the loaded keypair must equal
        // what's on disk — i.e. nothing was re-randomised during load.
        let ed_loaded: ed25519::Keypair = kp2
            .try_into_ed25519()
            .expect("loaded keypair should be ed25519");
        let loaded_secret = ed_loaded.secret();
        assert_eq!(
            AsRef::<[u8]>::as_ref(&loaded_secret),
            on_disk.as_slice(),
            "secret bytes returned by load must match the file on disk"
        );
    }
}
