//! Host-facing composition: signed snapshot tool binding → single checked source read.
//!
//! Combines extension `bind_handler_digest_from_signed` with `read_checked_source`.
//! Missing/None `handlerIntegrity` is fail-closed (`Digest(NotPresent)`); this path
//! never falls back to an unchecked read. Success returns the same owned verified
//! UTF-8 snapshot as `read_checked_source` (single-read integrity).
//!
//! Trust model unchanged: the Host must already trust the signed snapshot, supply
//! a trustworthy root/`/proc` view, and separately authorize any later execution.
//! This helper does not authenticate root/owner/curation and is not an execution
//! permit. Not M9/E13 completion.
use crate::extension_js_checked_source::{CheckedSourceError, IntegrityCheckedSource, read_checked_source};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use aionui_extension::hub::{HandlerDigestError, bind_handler_digest_from_signed};
use std::{fs::File, path::Path};

/// Fixed categories only; never include tool ids, paths, digests or source text.
#[derive(Debug, PartialEq, Eq)]
pub enum HandlerSourceBindError {
    Digest(HandlerDigestError),
    Checked(CheckedSourceError),
}

/// Resolve one tool's declared `(handler, expected SRI)` from a signature-checked
/// manifest snapshot, then read that handler once under a Host-trusted root and
/// compare against the extracted SRI.
///
/// Fail-closed: absence of `handlerIntegrity` → `Digest(NotPresent)`; ambiguous
/// tool id, missing tool, invalid re-parse, or empty handler → the corresponding
/// `HandlerDigestError`; SRI/path/reader failures → `Checked(...)`. No silent
/// unchecked fallback. Does not invoke handlers or expand routes/gates.
pub fn load_checked_handler_source_from_signed(
    checked: &SignatureCheckedManifest,
    tool_id: &str,
    root: &File,
) -> Result<IntegrityCheckedSource, HandlerSourceBindError> {
    let binding = bind_handler_digest_from_signed(checked, tool_id).map_err(HandlerSourceBindError::Digest)?;
    // `handler()` is non-empty when Ok (empty → HandlerMissing at bind time).
    read_checked_source(root, Path::new(binding.handler()), binding.integrity())
        .map_err(HandlerSourceBindError::Checked)
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::hub::HandlerDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        collections::{HashMap, HashSet},
        fs,
    };
    use tempfile::TempDir;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn sign(raw: &[u8], name: &str, version: &str) -> (String, String) {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        (integrity, signature)
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let (integrity, signature) = sign(raw, "demo.plugin", "1.0.0");
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    fn manifest_json(tools: &str) -> String {
        format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":{tools}}}}}"#)
    }

    fn fixture() -> (TempDir, File) {
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn happy_path_returns_exact_owned_snapshot() {
        let (temp, root) = fixture();
        let body = "export const run = () => '你好';\n";
        fs::create_dir_all(temp.path().join("handlers")).unwrap();
        fs::write(temp.path().join("handlers/search.js"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"handlers/search.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let source = load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());
        assert_eq!(source.into_source().into_bytes(), body.as_bytes());
    }

    #[test]
    fn missing_integrity_is_not_present_not_unchecked_read() {
        let (temp, root) = fixture();
        // File exists; composition must still refuse without declared integrity.
        fs::write(temp.path().join("handlers_search.js"), b"should-not-be-read").unwrap();
        let raw = manifest_json(r#"[{"id":"search","name":"Search","handler":"handlers_search.js"}]"#);
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).err(),
            Some(HandlerSourceBindError::Digest(HandlerDigestError::NotPresent))
        );
    }

    #[test]
    fn ambiguous_tool_id_fail_closed() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"A","handler":"a.js","handlerIntegrity":"{sri}"}},{{"id":"search","name":"B","handler":"b.js","handlerIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).err(),
            Some(HandlerSourceBindError::Digest(HandlerDigestError::AmbiguousToolId))
        );
    }

    #[test]
    fn unknown_tool_id_is_not_found() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "other", &root).err(),
            Some(HandlerSourceBindError::Digest(HandlerDigestError::ToolNotFound))
        );
    }

    #[test]
    fn hash_mismatch_when_file_differs_from_declared_sri() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("h.js"), b"actual-bytes").unwrap();
        let sri = sri_sha512(b"declared-other-bytes");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).err(),
            Some(HandlerSourceBindError::Checked(CheckedSourceError::IntegrityMismatch))
        );
    }

    #[test]
    fn bad_sri_rejected_before_composition_via_signed_check() {
        // Invalid present SRI fails parse_manifest inside check_signed_manifest;
        // composition never sees an unchecked path. Documented fail-closed gate.
        let raw = manifest_json(r#"[{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"sha256-x"}]"#);
        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let err = check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw.as_bytes(),
        )
        .err();
        assert_eq!(
            err,
            Some(aionui_extension::hub::signed_manifest::SignedManifestError::InvalidManifest)
        );
    }

    #[test]
    fn empty_handler_with_integrity_rejected_before_composition() {
        // validate_manifest requires non-empty handler when integrity is present.
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"","handlerIntegrity":"{sri}"}}]"#
        ));
        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let err = check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw.as_bytes(),
        )
        .err();
        assert_eq!(
            err,
            Some(aionui_extension::hub::signed_manifest::SignedManifestError::InvalidManifest)
        );
    }

    #[test]
    fn missing_handler_file_surfaces_reader_error() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"missing.js","handlerIntegrity":"{sri}"}}]"#
        ));
        match load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).err() {
            Some(HandlerSourceBindError::Checked(CheckedSourceError::Reader(_))) => {}
            other => panic!("expected Checked(Reader(_)), got {other:?}"),
        }
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    use aionui_extension::hub::HandlerDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    #[test]
    fn missing_integrity_still_digest_not_present_when_disabled() {
        // Digest stage runs before platform gate; never unchecked fallback.
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"agentTools":[{"id":"search","name":"Search","handler":"h.js"}]}}"#;
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw), "search", &root).err(),
            Some(HandlerSourceBindError::Digest(HandlerDigestError::NotPresent))
        );
    }

    #[test]
    fn present_binding_is_unavailable_when_disabled_not_unchecked() {
        let body = b"export const run = () => 1;";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]}}}}"#
        );
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_handler_source_from_signed(&checked(raw.as_bytes()), "search", &root).err(),
            Some(HandlerSourceBindError::Checked(CheckedSourceError::Unavailable))
        );
    }
}
