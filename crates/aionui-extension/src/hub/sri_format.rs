//! Canonical SHA-512 SRI string format shared by handler digests and app checks.
//!
//! This validates the **syntax** of an expected digest only. It does not prove
//! the digest was curated, signed or bound to any filesystem content by itself.

/// Length of a canonical `sha512-<standard-base64>` string (7 + 88).
pub const CANONICAL_SHA512_SRI_LEN: usize = 95;

/// Return true iff `value` is exactly the canonical SHA-512 SRI encoding.
///
/// Rules match the experimental checked-source API:
/// - exactly 95 UTF-8 bytes
/// - lowercase `sha512-` prefix
/// - 86 standard Base64 alphabet characters (`A-Za-z0-9+/`)
/// - exactly `==` padding
/// - final data sextet has zero padding bits (`A`/`Q`/`g`/`w` only)
///
/// No trimming, algorithm aliases, URL-safe alphabet or case folding. Length is
/// checked first so malformed caller input cannot force unbounded work.
pub fn is_canonical_sha512_sri(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != CANONICAL_SHA512_SRI_LEN || !bytes.starts_with(b"sha512-") {
        return false;
    }
    // 64 digest bytes = 21*3 + 1 => 86 data chars + `==`. The last sextet keeps
    // only two significant bits; its four padding bits MUST be zero.
    bytes[93..] == *b"=="
        && bytes[7..93]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(b))
        && matches!(bytes[92], b'A' | b'Q' | b'g' | b'w')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::sri_sha512;

    #[test]
    fn accepts_helper_outputs_and_every_valid_padding_sextet() {
        for byte in 0..=255 {
            assert!(is_canonical_sha512_sri(&sri_sha512(&[byte])));
        }
        let mut candidate = sri_sha512(b"source").into_bytes();
        for last in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/" {
            candidate[92] = *last;
            let ok = is_canonical_sha512_sri(std::str::from_utf8(&candidate).unwrap());
            assert_eq!(ok, matches!(*last, b'A' | b'Q' | b'g' | b'w'));
        }
    }

    #[test]
    fn rejects_common_malformed_forms() {
        let good = sri_sha512(b"");
        let mut noncanonical = good.as_bytes().to_vec();
        noncanonical[92] = b'B';
        let mut invalid_char = good.as_bytes().to_vec();
        invalid_char[7] = b'_';
        for bad in [
            String::new(),
            "x".repeat(1024),
            good.replacen("sha512-", "sha256-", 1),
            good.replacen("sha512-", "SHA512-", 1),
            good.trim_end_matches('=').to_owned(),
            format!("{good} "),
            format!(" {good}"),
            good.replace("==", "=A"),
            String::from_utf8(noncanonical).unwrap(),
            String::from_utf8(invalid_char).unwrap(),
            "sha512-".to_owned() + &"é".repeat(44),
        ] {
            assert!(!is_canonical_sha512_sri(&bad), "{bad:?}");
        }
    }
}
