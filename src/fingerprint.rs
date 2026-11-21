//! A small, non-cryptographic content hash (FNV-1a, 64 bit).
//!
//! Used to notice that something *changed* (a devcontainer's inputs, a library
//! that was updated on the host), never to defend against an adversary.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A running hash: feed it bytes, then read it out.
#[derive(Debug, Clone, Copy)]
pub struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Fingerprint {
    pub fn new() -> Self {
        Self(FNV_OFFSET)
    }

    /// Folds `data` in as is.
    pub fn put(&mut self, data: &[u8]) -> &mut Self {
        for &b in data {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
        self
    }

    /// Folds `data` in followed by a NUL, so consecutive fields can't run into
    /// each other (`"ab","c"` differs from `"a","bc"`).
    pub fn field(&mut self, data: &[u8]) -> &mut Self {
        self.put(data).put(&[0])
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    /// The hash as 16 lowercase hex digits.
    pub fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// The hash of `data` as 16 lowercase hex digits.
pub fn content_hash(data: &[u8]) -> String {
    Fingerprint::new().put(data).hex()
}

#[cfg(test)]
mod tests {
    use super::content_hash;

    #[test]
    fn equal_bytes_hash_equal_and_different_bytes_differ() {
        assert_eq!(content_hash(b"libc 2.40"), content_hash(b"libc 2.40"));
        assert_ne!(content_hash(b"libc 2.40"), content_hash(b"libc 2.41"));
    }

    #[test]
    fn hash_is_sixteen_hex_digits() {
        let h = content_hash(b"");
        assert_eq!(h.len(), 16);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
