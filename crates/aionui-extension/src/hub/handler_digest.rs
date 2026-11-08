//! Optional per-tool handler digest extraction from typed/signed manifests.
//!
//! A present, valid `handlerIntegrity` value is only a **declared expected SRI**
//! inside already-trusted manifest bytes. Missing/None never means verified.
//! Combining this with `SignatureCheckedManifest` + app `read_checked_source`
//! can form a host-mediated binding; this module alone does not authorize
//! execution, prove curation/owner/root or read handler files.
use super::signed_manifest::SignatureCheckedManifest;
use super::sri_format::is_canonical_sha512_sri;
use crate::manifest::parse_manifest;
use crate::types::ExtensionManifest;

/// Fixed categories; never include digests, paths, tool ids or raw JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerDigestError {
    ToolNotFound,
    AmbiguousToolId,
    NotPresent,
    HandlerMissing,
    InvalidIntegrity,
    InvalidManifest,
}

/// Exact handler relative path and expected canonical SRI copied from the manifest.
///
/// Private fields with borrowing getters. This is not an execution permit.
#[derive(Clone, PartialEq, Eq)]
pub struct HandlerDigestBinding {
    handler: String,
    integrity: String,
}

impl std::fmt::Debug for HandlerDigestBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Summary only: never dump handler path or digest bytes into logs/asserts.
        f.write_str("HandlerDigestBinding")
    }
}

impl HandlerDigestBinding {
    pub fn handler(&self) -> &str {
        &self.handler
    }

    pub fn integrity(&self) -> &str {
        &self.integrity
    }
}

/// Look up one agent tool by **exact `id`** and return its declared digest binding.
///
/// Precedence is exact tool id only (not handler path). Duplicate ids among
/// `contributes.agent_tools` fail closed with `AmbiguousToolId` because the pure
/// JSON array parser keeps every element. Missing field => `NotPresent` (not Ok).
/// Present invalid SRI => `InvalidIntegrity`. Present without a non-empty
/// `handler` => `HandlerMissing`. No digest is invented.
pub fn expected_handler_integrity(
    manifest: &ExtensionManifest,
    tool_id: &str,
) -> Result<HandlerDigestBinding, HandlerDigestError> {
    let tools = manifest
        .contributes
        .as_ref()
        .map(|c| c.agent_tools.as_slice())
        .unwrap_or(&[]);
    let matches: Vec<_> = tools.iter().filter(|tool| tool.id == tool_id).collect();
    match matches.as_slice() {
        [] => Err(HandlerDigestError::ToolNotFound),
        [_, _, ..] => Err(HandlerDigestError::AmbiguousToolId),
        [tool] => {
            let Some(integrity) = tool.handler_integrity.as_deref() else {
                return Err(HandlerDigestError::NotPresent);
            };
            if !is_canonical_sha512_sri(integrity) {
                return Err(HandlerDigestError::InvalidIntegrity);
            }
            let Some(handler) = tool.handler.as_deref().filter(|h| !h.is_empty()) else {
                return Err(HandlerDigestError::HandlerMissing);
            };
            Ok(HandlerDigestBinding {
                handler: handler.to_owned(),
                integrity: integrity.to_owned(),
            })
        }
    }
}

/// Re-parse a signature-checked manifest **byte snapshot** and extract one tool binding.
///
/// Uses the owned raw bytes only — no filesystem reread. Success still requires
/// a present valid field; absence is `NotPresent`, never a synthetic Ok digest.
/// Host trust in the signed snapshot and later `read_checked_source` comparison
/// remain separate. Duplicate JSON object keys follow serde_json Value last-wins
/// during parse; duplicate array entries with the same tool id fail closed here.
pub fn bind_handler_digest_from_signed(
    checked: &SignatureCheckedManifest,
    tool_id: &str,
) -> Result<HandlerDigestBinding, HandlerDigestError> {
    let manifest = parse_manifest(checked.raw_bytes()).map_err(|_| HandlerDigestError::InvalidManifest)?;
    expected_handler_integrity(&manifest, tool_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
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

    #[test]
    fn missing_field_is_not_present_not_ok() {
        let raw = manifest_json(r#"[{"id":"search","name":"Search","handler":"handlers/search.js"}]"#);
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(
            expected_handler_integrity(&manifest, "search"),
            Err(HandlerDigestError::NotPresent)
        );
        assert_eq!(
            bind_handler_digest_from_signed(&checked(raw.as_bytes()), "search"),
            Err(HandlerDigestError::NotPresent)
        );
    }

    #[test]
    fn present_valid_returns_exact_handler_and_sri() {
        let sri = sri_sha512(b"export const run = () => 1;");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"handlers/search.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let binding = bind_handler_digest_from_signed(&checked(raw.as_bytes()), "search").unwrap();
        assert_eq!(binding.handler(), "handlers/search.js");
        assert_eq!(binding.integrity(), sri);
        assert_eq!(binding.integrity().as_bytes(), sri.as_bytes());
    }

    #[test]
    fn camelcase_alias_and_legacy_plan_safe_still_parse() {
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"t","name":"T","planSafeActions":true,"handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let tool = &manifest.contributes.as_ref().unwrap().agent_tools[0];
        assert!(tool.plan_safe_actions);
        assert_eq!(tool.handler_integrity.as_deref(), Some(sri.as_str()));
        assert_eq!(expected_handler_integrity(&manifest, "t").unwrap().handler(), "h.js");
    }

    #[test]
    fn invalid_present_sri_rejected_at_parse() {
        let raw = manifest_json(r#"[{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"sha256-x"}]"#);
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn malformed_sri_forms_rejected() {
        let good = sri_sha512(b"code");
        let mut bad_pad = good.as_bytes().to_vec();
        bad_pad[92] = b'B';
        for sri in [
            String::new(),
            good.replacen("sha512-", "SHA512-", 1),
            format!("{good} "),
            String::from_utf8(bad_pad).unwrap(),
            "sha512-".to_owned() + &"A".repeat(88),
        ] {
            let raw = manifest_json(&format!(
                r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
            ));
            assert!(parse_manifest(raw.as_bytes()).is_err(), "{sri:?}");
        }
    }

    #[test]
    fn integrity_without_handler_rejected() {
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handlerIntegrity":"{sri}"}}]"#
        ));
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn wrong_tool_and_unknown_tool() {
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let checked = checked(raw.as_bytes());
        assert_eq!(
            bind_handler_digest_from_signed(&checked, "other"),
            Err(HandlerDigestError::ToolNotFound)
        );
    }

    #[test]
    fn duplicate_tool_ids_fail_closed_for_binder() {
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"A","handler":"a.js","handlerIntegrity":"{sri}"}},{{"id":"search","name":"B","handler":"b.js","handlerIntegrity":"{sri}"}}]"#
        ));
        // Pure array parse keeps both elements; binder must not silently pick one.
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(manifest.contributes.as_ref().unwrap().agent_tools.len(), 2);
        assert_eq!(
            expected_handler_integrity(&manifest, "search"),
            Err(HandlerDigestError::AmbiguousToolId)
        );
        assert_eq!(
            bind_handler_digest_from_signed(&checked(raw.as_bytes()), "search"),
            Err(HandlerDigestError::AmbiguousToolId)
        );
    }

    #[test]
    fn duplicate_object_key_last_wins_is_documented_by_parse() {
        // serde_json::Value keeps the last duplicate object key before typing.
        let sri_first = sri_sha512(b"first");
        let sri_last = sri_sha512(b"last");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri_first}","handlerIntegrity":"{sri_last}"}}]}}}}"#
        );
        let binding = bind_handler_digest_from_signed(&checked(raw.as_bytes()), "search").unwrap();
        assert_eq!(binding.integrity(), sri_last);
        assert_ne!(binding.integrity(), sri_first);
    }

    #[test]
    fn backcompat_manifest_without_field_still_parses() {
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"agentTools":[{"id":"search","name":"Search","handler":"handlers/search.js"}]}}"#;
        let manifest = parse_manifest(raw).unwrap();
        assert!(
            manifest.contributes.as_ref().unwrap().agent_tools[0]
                .handler_integrity
                .is_none()
        );
    }
}
