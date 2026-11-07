//! Strict verification of a bounded manifest-byte snapshot, not authorization.
//!
//! The host must supply a trusted policy/key map, never keys obtained from the
//! manifest, index or request being checked. No install-gate fallback is used.
use super::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
use crate::manifest::parse_manifest;
use ed25519_dalek::Signature;

/// Maximum original manifest byte length (256 KiB), before hashing or parsing.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Limits apply to UTF-8 byte lengths, not character counts.
pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_VERSION_BYTES: usize = 128;
pub const MAX_KEY_ID_BYTES: usize = 128;

/// Borrowed, minimal input projection; does not expose the private hub index type.
///
/// Signature encoding is the existing 128-character hex encoding of 64 Ed25519
/// signature bytes. No base64 signature fallback, trimming or auto-detection.
/// Flags such as bundled, curated and optional signatures are deliberately absent.
pub struct SignedManifestEntry<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub pubkey_id: Option<&'a str>,
    pub integrity: Option<&'a str>,
    pub signature: Option<&'a str>,
}

/// Fixed categories only: never include manifest, path, environment or key data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedManifestError {
    ManifestLimit,
    MetadataLimit,
    InvalidIdentity,
    NotAllowlisted,
    MissingKeyId,
    InvalidKeyId,
    UnknownKey,
    MissingIntegrity,
    InvalidIntegrity,
    IntegrityMismatch,
    MissingSignature,
    InvalidSignature,
    InvalidManifest,
    IdentityMismatch,
}

/// Owned snapshot of exactly the checked bytes, not an execution/install permit.
///
/// No unchecked constructor, Default, Deserialize or raw-data Debug formatter.
/// Consumers must use this snapshot, not reread a path after verification.
/// This does not attest handler bytes, root/owner, curation or future key validity.
pub struct SignatureCheckedManifest {
    raw: Box<[u8]>,
    name: String,
    version: String,
    key_id: String,
}

impl SignatureCheckedManifest {
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

fn identity(value: &str, limit: usize) -> Result<(), SignedManifestError> {
    if value.len() > limit {
        return Err(SignedManifestError::MetadataLimit);
    }
    // In particular reject LF/CR before constructing the newline-delimited
    // canonical payload, so fields cannot change its serialization boundaries.
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(SignedManifestError::InvalidIdentity);
    }
    Ok(())
}

/// Verify original bytes using the host's explicit trusted policy snapshot.
///
/// A known key, signature and exact canonical sha512 SRI are required on EVERY
/// call, even if policy.require_signature is false. A nonempty allowlist is
/// respected with no bundled bypass. No global/env policy is acquired here.
/// The pure parse_manifest parser normalizes legacy keys and validates existing
/// name/reserved-prefix/semver rules; parsed name/version must exactly match the
/// signed entry. This is not full capability/contribution/schema authorization.
/// There is no file IO, $file resolution, activation or registry/route wiring.
pub fn check_signed_manifest(
    policy: &CuratedInstallPolicy,
    entry: &SignedManifestEntry<'_>,
    raw: &[u8],
) -> Result<SignatureCheckedManifest, SignedManifestError> {
    if raw.len() > MAX_MANIFEST_BYTES {
        return Err(SignedManifestError::ManifestLimit);
    }
    identity(entry.name, MAX_NAME_BYTES)?;
    identity(entry.version, MAX_VERSION_BYTES)?;
    let key_id = entry.pubkey_id.ok_or(SignedManifestError::MissingKeyId)?;
    if key_id.len() > MAX_KEY_ID_BYTES {
        return Err(SignedManifestError::MetadataLimit);
    }
    if key_id.is_empty() || !key_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
        return Err(SignedManifestError::InvalidKeyId);
    }
    // Bounded lookup of a host-configured key. No empty-map warning/skip path.
    let key = policy.pubkeys.get(key_id).ok_or(SignedManifestError::UnknownKey)?;
    if !policy.allowlist.is_empty() && !policy.allowlist.contains(entry.name) {
        return Err(SignedManifestError::NotAllowlisted);
    }
    let integrity = entry.integrity.ok_or(SignedManifestError::MissingIntegrity)?;
    // sha512- (7) + canonical padded base64 SHA-512 (88) = exactly 95 bytes.
    // Exact equality with our computed SRI below also rejects wrong algorithms,
    // malformed/noncanonical encodings, digest case mutations and whitespace.
    if integrity.len() != 95 || !integrity.starts_with("sha512-") {
        return Err(SignedManifestError::InvalidIntegrity);
    }
    let signature = entry.signature.ok_or(SignedManifestError::MissingSignature)?;
    if signature.len() != 128 {
        return Err(SignedManifestError::InvalidSignature);
    }
    let mut decoded = [0u8; 64];
    // Fixed-size decoding: attacker input cannot cause unbounded allocation.
    hex::decode_to_slice(signature, &mut decoded).map_err(|_| SignedManifestError::InvalidSignature)?;
    if sri_sha512(raw) != integrity {
        return Err(SignedManifestError::IntegrityMismatch);
    }
    let payload = signature_payload(entry.name, entry.version, integrity);
    key.verify_strict(payload.as_bytes(), &Signature::from_bytes(&decoded))
        .map_err(|_| SignedManifestError::InvalidSignature)?;
    // Hash/signature bind ORIGINAL bytes, never a normalized/reserialized value.
    // parse_manifest(None-dir) is pure and does not resolve $file references.
    let parsed = parse_manifest(raw).map_err(|_| SignedManifestError::InvalidManifest)?;
    if parsed.name != entry.name || parsed.version != entry.version {
        return Err(SignedManifestError::IdentityMismatch);
    }
    Ok(SignatureCheckedManifest {
        raw: raw.into(),
        name: parsed.name,
        version: parsed.version,
        key_id: key_id.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
    use std::collections::{HashMap, HashSet};

    const RAW: &[u8] = b" \n{\"name\":\"demo.plugin\",\"version\":\"1.2.3\",\"displayName\":\"Demo\",\"entryPoint\":\"$file:never-read.js\"}\n";

    struct Entry {
        name: String,
        version: String,
        integrity: String,
        signature: String,
    }
    impl Entry {
        fn signed(raw: &[u8], name: &str, version: &str) -> Self {
            let integrity = sri_sha512(raw);
            let signature = hex::encode(
                signing_key()
                    .sign(signature_payload(name, version, &integrity).as_bytes())
                    .to_bytes(),
            );
            Self {
                name: name.into(),
                version: version.into(),
                integrity,
                signature,
            }
        }
        fn input(&self) -> SignedManifestEntry<'_> {
            SignedManifestEntry {
                name: &self.name,
                version: &self.version,
                pubkey_id: Some("fixture"),
                integrity: Some(&self.integrity),
                signature: Some(&self.signature),
            }
        }
    }
    fn signing_key() -> SigningKey {
        // Public deterministic test fixture; not an operational secret.
        SigningKey::from_bytes(&[7; 32])
    }
    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }
    fn error(policy: &CuratedInstallPolicy, input: &SignedManifestEntry<'_>, raw: &[u8]) -> SignedManifestError {
        check_signed_manifest(policy, input, raw).err().expect("must reject")
    }

    #[test]
    fn preserves_original_owned_bytes_and_exact_identity() {
        let mut raw = RAW.to_vec();
        let entry = Entry::signed(&raw, "demo.plugin", "1.2.3");
        let checked = check_signed_manifest(&policy(), &entry.input(), &raw).unwrap();
        raw.fill(b'x'); // success owns a distinct snapshot, not the caller buffer
        assert_eq!(checked.raw_bytes(), RAW);
        assert_eq!(
            (checked.name(), checked.version(), checked.key_id()),
            ("demo.plugin", "1.2.3", "fixture")
        );
        // Legacy camelCase normalization and literal $file do not rewrite the
        // signed snapshot, read a file, or grant authority to execute its entry.
        let parsed = parse_manifest(checked.raw_bytes()).unwrap();
        assert_eq!(parsed.display_name.as_deref(), Some("Demo"));
        assert_eq!(parsed.entry_point.as_deref(), Some("$file:never-read.js"));
    }

    #[test]
    fn always_requires_known_host_key_even_when_signature_policy_optional() {
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let mut input = entry.input();
        let p = policy();
        assert!(!p.require_signature); // no bundled/curated flags exist in input
        input.pubkey_id = None;
        assert_eq!(error(&p, &input, RAW), SignedManifestError::MissingKeyId);
        for id in ["", "bad key", "key\nother"] {
            input.pubkey_id = Some(id);
            assert_eq!(error(&p, &input, RAW), SignedManifestError::InvalidKeyId);
        }
        input.pubkey_id = Some("unknown");
        assert_eq!(error(&p, &input, RAW), SignedManifestError::UnknownKey);
        input.pubkey_id = Some("fixture");
        let mut empty = policy();
        empty.pubkeys.clear();
        assert_eq!(error(&empty, &input, RAW), SignedManifestError::UnknownKey);
        for required in [false, true] {
            let mut p = policy();
            p.require_signature = required;
            input.signature = None;
            assert_eq!(error(&p, &input, RAW), SignedManifestError::MissingSignature);
            input.signature = Some("");
            assert_eq!(error(&p, &input, RAW), SignedManifestError::InvalidSignature);
            input.signature = Some(&entry.signature);
            input.integrity = None;
            assert_eq!(error(&p, &input, RAW), SignedManifestError::MissingIntegrity);
            input.integrity = Some("");
            assert_eq!(error(&p, &input, RAW), SignedManifestError::InvalidIntegrity);
            input.integrity = Some(&entry.integrity);
        }
    }

    #[test]
    fn honors_only_explicit_allowlist_without_bypass() {
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let mut p = policy();
        p.allowlist.insert("other.plugin".into());
        assert_eq!(error(&p, &entry.input(), RAW), SignedManifestError::NotAllowlisted);
        p.allowlist.insert("demo.plugin".into());
        assert!(check_signed_manifest(&p, &entry.input(), RAW).is_ok());
    }

    #[test]
    fn rejects_wrong_key_signature_and_manifest_tampering() {
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let mut wrong = policy();
        wrong
            .pubkeys
            .insert("fixture".into(), SigningKey::from_bytes(&[8; 32]).verifying_key());
        assert_eq!(
            error(&wrong, &entry.input(), RAW),
            SignedManifestError::InvalidSignature
        );
        let mut signature = hex::decode(&entry.signature).unwrap();
        signature[0] ^= 1;
        let signature = hex::encode(signature);
        let mut input = entry.input();
        input.signature = Some(&signature);
        assert_eq!(error(&policy(), &input, RAW), SignedManifestError::InvalidSignature);
        let mut raw = RAW.to_vec();
        raw.push(b' '); // even valid-JSON whitespace changes the signed digest
        assert_eq!(
            error(&policy(), &entry.input(), &raw),
            SignedManifestError::IntegrityMismatch
        );
    }

    #[test]
    fn canonical_sri_only_including_digest_case() {
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let mut mutated = entry.integrity.clone().into_bytes();
        let letter = mutated[7..].iter().position(u8::is_ascii_alphabetic).unwrap() + 7;
        mutated[letter] ^= 0x20;
        let mutated = String::from_utf8(mutated).unwrap();
        let mut input = entry.input();
        input.integrity = Some(&mutated);
        assert_eq!(error(&policy(), &input, RAW), SignedManifestError::IntegrityMismatch);
        for sri in [
            "sha256-x".to_owned(),
            entry.integrity.replacen("sha512-", "SHA512-", 1),
            format!("{} ", entry.integrity),
            "sha512-".to_owned() + &"!".repeat(88),
        ] {
            let mut input = entry.input();
            input.integrity = Some(&sri);
            assert!(matches!(
                error(&policy(), &input, RAW),
                SignedManifestError::InvalidIntegrity | SignedManifestError::IntegrityMismatch
            ));
        }
    }

    #[test]
    fn signed_identity_must_match_parsed_manifest_exactly() {
        for (name, version) in [
            ("other.plugin", "1.2.3"),
            ("demo.plugin", "1.2.4"),
            ("Demo.Plugin", "1.2.3"),
        ] {
            let entry = Entry::signed(RAW, name, version); // valid signature for the WRONG identity
            assert_eq!(
                error(&policy(), &entry.input(), RAW),
                SignedManifestError::IdentityMismatch
            );
        }
    }

    #[test]
    fn pure_parser_rejects_bad_json_and_existing_contract_violations() {
        for raw in [
            b"not json".as_slice(),
            b"{\"name\":\"demo.plugin\",\"version\":\"invalid\"}",
            b"{\"name\":\"aion-reserved\",\"version\":\"1.2.3\"}",
        ] {
            let entry = Entry::signed(raw, "demo.plugin", "1.2.3");
            assert_eq!(
                error(&policy(), &entry.input(), raw),
                SignedManifestError::InvalidManifest
            );
        }
    }

    #[test]
    fn original_raw_byte_limit_accepts_exact_boundary_rejects_plus_one() {
        let mut raw = RAW.to_vec();
        raw.resize(MAX_MANIFEST_BYTES, b' ');
        let entry = Entry::signed(&raw, "demo.plugin", "1.2.3");
        assert_eq!(
            check_signed_manifest(&policy(), &entry.input(), &raw)
                .unwrap()
                .raw_bytes(),
            raw
        );
        raw.push(b' ');
        assert_eq!(
            error(&policy(), &entry.input(), &raw),
            SignedManifestError::ManifestLimit
        );
    }

    #[test]
    fn metadata_byte_boundaries_and_control_characters() {
        let name = "n".repeat(MAX_NAME_BYTES);
        let version = "1.2.3+".to_owned() + &"a".repeat(MAX_VERSION_BYTES - 6);
        let id = "k".repeat(MAX_KEY_ID_BYTES);
        let raw = serde_json::to_vec(&serde_json::json!({"name":name,"version":version})).unwrap();
        let entry = Entry::signed(&raw, &name, &version);
        let mut p = policy();
        p.pubkeys.insert(id.clone(), signing_key().verifying_key());
        let mut input = entry.input();
        input.pubkey_id = Some(&id);
        assert!(check_signed_manifest(&p, &input, &raw).is_ok());
        let too_long = [
            "n".repeat(MAX_NAME_BYTES + 1),
            "v".repeat(MAX_VERSION_BYTES + 1),
            "k".repeat(MAX_KEY_ID_BYTES + 1),
        ];
        for field in 0..3 {
            let mut input = entry.input();
            match field {
                0 => input.name = &too_long[0],
                1 => input.version = &too_long[1],
                _ => input.pubkey_id = Some(&too_long[2]),
            }
            assert_eq!(error(&p, &input, &raw), SignedManifestError::MetadataLimit);
        }
        for value in ["", "x\ny", "x\ry", "x\0y", "x\ty", "x\u{85}y"] {
            let mut input = entry.input();
            input.name = value;
            assert_eq!(error(&p, &input, &raw), SignedManifestError::InvalidIdentity);
            input.name = &entry.name;
            input.version = value;
            assert_eq!(error(&p, &input, &raw), SignedManifestError::InvalidIdentity);
        }
    }

    #[test]
    fn malformed_hex_signature_lengths_and_no_base64_fallback() {
        use base64::Engine;
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let b64 = base64::engine::general_purpose::STANDARD.encode(hex::decode(&entry.signature).unwrap());
        let bad = [
            "0".repeat(127),
            "0".repeat(129),
            "g".repeat(128),
            format!(" {}", entry.signature),
            b64,
            "f".repeat(1024 * 1024),
        ];
        for signature in bad {
            let mut input = entry.input();
            input.signature = Some(&signature);
            assert_eq!(error(&policy(), &input, RAW), SignedManifestError::InvalidSignature);
        }
    }

    #[test]
    fn strict_verification_rejects_weak_identity_key_and_invalid_signature() {
        // Known encoded identity point: verifies only strict rejection here,
        // not a claim about exploitability of any production key configuration.
        let mut identity_point = [0; 32];
        identity_point[0] = 1;
        let weak = VerifyingKey::from_bytes(&identity_point).unwrap();
        assert!(weak.is_weak());
        let mut p = policy();
        p.pubkeys.insert("fixture".into(), weak);
        let entry = Entry::signed(RAW, "demo.plugin", "1.2.3");
        let mut forged = [0u8; 64];
        forged[..32].copy_from_slice(&identity_point);
        let signature = hex::encode(forged);
        let mut input = entry.input();
        input.signature = Some(&signature);
        assert_eq!(error(&p, &input, RAW), SignedManifestError::InvalidSignature);
    }
}
