//! A small, stable, non-cryptographic content hash (FNV-1a, 64 bit).
//!
//! Used to notice that something *changed* (a devcontainer's inputs, a library
//! that was updated on the host), never to defend against an adversary.

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Folds `data` into the running hash `h`. Start from [`FNV_OFFSET`].
pub fn fnv1a(data: &[u8], mut h: u64) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The hash of `data` as 16 lowercase hex digits.
pub fn content_hash(data: &[u8]) -> String {
    format!("{:016x}", fnv1a(data, FNV_OFFSET))
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
