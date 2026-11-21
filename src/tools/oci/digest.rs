//! A content digest (`sha256:<64 hex>`), and checking bytes against one. The only
//! file that names the hash crate.

use anyhow::{Result, bail};
use sha2::{Digest as _, Sha256};
use std::io::Read;

/// A sha256 digest, as registries name content. Other algorithms are refused
/// rather than trusted unchecked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Self> {
        let Some(hex) = s.strip_prefix("sha256:") else {
            bail!("unsupported digest {s:?} (only sha256 is)");
        };
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            bail!("malformed digest {s:?}");
        }
        Ok(Self(s.to_string()))
    }

    /// `sha256:<hex>`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A name that is safe as a file name: `sha256-<hex>`.
    pub fn file_name(&self) -> String {
        self.0.replace(':', "-")
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex(&Sha256::digest(bytes))))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copies `from` to `to`, at most `size` bytes, and checks that it was exactly
/// `size` bytes and hashes to `expected`. Nothing past `size` is read, so a
/// registry cannot send more than the manifest declared.
pub fn copy_verified(
    from: &mut dyn Read,
    to: &mut dyn std::io::Write,
    size: u64,
    expected: &Digest,
) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut limited = from.take(size.saturating_add(1));
    let mut total = 0u64;
    loop {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        total += n as u64;
    }
    if total != size {
        bail!("{expected} is {total} bytes, the manifest says {size}");
    }
    let got = Digest(format!("sha256:{}", hex(&hasher.finalize())));
    if &got != expected {
        bail!("content is {got}, expected {expected}");
    }
    Ok(())
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_of_known_content() {
        assert_eq!(
            Digest::of(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn only_well_formed_sha256_is_accepted() {
        assert!(Digest::parse(Digest::of(b"x").as_str()).is_ok());
        assert!(Digest::parse("sha512:abc").is_err());
        assert!(Digest::parse("sha256:ABC").is_err());
        assert!(Digest::parse(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert_eq!(
            Digest::of(b"x").file_name().split_once('-').unwrap().0,
            "sha256"
        );
    }

    #[test]
    fn a_copy_is_checked_for_size_and_content() {
        let want = Digest::of(b"hello");
        let mut out = Vec::new();
        copy_verified(&mut &b"hello"[..], &mut out, 5, &want).unwrap();
        assert_eq!(out, b"hello");
        let wrong = copy_verified(&mut &b"jello"[..], &mut Vec::new(), 5, &want).unwrap_err();
        assert!(wrong.to_string().contains("expected"), "{wrong}");
        let long = copy_verified(&mut &b"hello!"[..], &mut Vec::new(), 5, &want).unwrap_err();
        assert!(long.to_string().contains("manifest says 5"), "{long}");
        let short = copy_verified(&mut &b"hell"[..], &mut Vec::new(), 5, &want).unwrap_err();
        assert!(short.to_string().contains("manifest says 5"), "{short}");
    }
}
