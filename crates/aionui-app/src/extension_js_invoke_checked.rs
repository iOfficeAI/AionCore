//! Host invoke prep once a signed snapshot + trusted root are available.
//!
//! Wraps `load_checked_handler_source_from_signed` into a fail-closed decision
//! the agent-tool invoke path can call later: Ok → non-executing integrity-checked
//! ack metadata (source stays Host-private); Err → refuse (never `handler_stub`
//! success). Missing `handlerIntegrity` is Digest `NotPresent` / refuse — never
//! dispatched.
//!
//! HTTP routes today do **not** hold `SignatureCheckedManifest` or a Host-trusted
//! root (`ExtensionRouterState` is registry + host_runtime only). Do not invent
//! those from `LoadedExtension.directory` or re-parsed unsigned manifests. Until
//! Host plumbing exists, the extension route must refuse with
//! `integrity_binding_unavailable` rather than fake a checked load.
//!
//! Not execution authorization, not M9/E13 completion, not a Node/shell unlock.
use crate::extension_js_checked_source::IntegrityCheckedSource;
use crate::extension_js_handler_source::{HandlerSourceBindError, load_checked_handler_source_from_signed};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use std::fs::File;

/// Fixed refuse categories for invoke prep; never include source, paths, digests,
/// tool ids, or raw error text suitable for logs/HTTP leakage.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckedInvokeRefuse {
    Bind(HandlerSourceBindError),
}

impl CheckedInvokeRefuse {
    /// Stable HTTP/ack `reason` token. Categories only — no sensitive payloads.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Bind(HandlerSourceBindError::Digest(aionui_extension::hub::HandlerDigestError::NotPresent)) => {
                "handler_integrity_not_present"
            }
            Self::Bind(HandlerSourceBindError::Digest(_)) => "handler_digest_bind_failed",
            Self::Bind(HandlerSourceBindError::Checked(_)) => "handler_source_check_failed",
        }
    }
}

/// Successful integrity-checked prepare. Holds the verified source for Host use
/// only; ack helpers never serialize source text.
pub struct CheckedInvokePrepared {
    source: IntegrityCheckedSource,
}

impl std::fmt::Debug for CheckedInvokePrepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckedInvokePrepared")
    }
}

impl CheckedInvokePrepared {
    /// Non-executing ack status after a successful checked load.
    pub const ACK_STATUS: &str = "accepted";
    /// Reason token proving integrity was checked (no source leaked).
    pub const ACK_REASON: &str = "handler_integrity_checked";

    /// Always false: checked load is not execution / dispatch.
    pub fn dispatched(&self) -> bool {
        false
    }

    pub fn ack_status(&self) -> &'static str {
        Self::ACK_STATUS
    }

    pub fn ack_reason(&self) -> &'static str {
        Self::ACK_REASON
    }

    /// Transfer the Host-private verified snapshot. Separate authorization still
    /// required before any experimental JS supervisor invoke.
    pub fn into_checked_source(self) -> IntegrityCheckedSource {
        self.source
    }
}

/// Reason used when Host cannot supply signed snapshot + trusted root at all.
/// Extension routes should refuse with this rather than `handler_stub` success.
pub const INTEGRITY_BINDING_UNAVAILABLE_REASON: &str = "integrity_binding_unavailable";

/// Fail-closed prepare for agent-tool invoke given Host-trusted inputs.
///
/// On `Ok`, source matched the signed tool binding under the supplied root; the
/// returned value is suitable for a non-executing ack (`dispatched=false`,
/// reason `handler_integrity_checked`). On `Err`, do not fall back to stub
/// success — map `reason()` into the refuse ack. Never logs or returns source
/// text. Does not expand HTTP surface, permissions, or plan-safe gates.
pub fn prepare_checked_agent_tool_invoke(
    checked: &SignatureCheckedManifest,
    tool_id: &str,
    root: &File,
) -> Result<CheckedInvokePrepared, CheckedInvokeRefuse> {
    let source = load_checked_handler_source_from_signed(checked, tool_id, root).map_err(CheckedInvokeRefuse::Bind)?;
    Ok(CheckedInvokePrepared { source })
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
        SigningKey::from_bytes(&[13; 32])
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
    fn ok_ack_is_non_executing_and_keeps_source_host_private() {
        let (temp, root) = fixture();
        let body = "export const run = () => 1;\n";
        fs::write(temp.path().join("h.js"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let prepared = prepare_checked_agent_tool_invoke(&checked(raw.as_bytes()), "search", &root).unwrap();
        assert_eq!(prepared.ack_status(), "accepted");
        assert_eq!(prepared.ack_reason(), "handler_integrity_checked");
        assert!(!prepared.dispatched());
        // Debug must not dump source text.
        assert_eq!(format!("{prepared:?}"), "CheckedInvokePrepared");
        assert_eq!(prepared.into_checked_source().as_str(), body);
    }

    #[test]
    fn missing_integrity_refuses_not_present_not_stub() {
        let (_temp, root) = fixture();
        let raw = manifest_json(r#"[{"id":"search","name":"Search","handler":"h.js"}]"#);
        let err = prepare_checked_agent_tool_invoke(&checked(raw.as_bytes()), "search", &root)
            .err()
            .unwrap();
        assert_eq!(
            err,
            CheckedInvokeRefuse::Bind(HandlerSourceBindError::Digest(HandlerDigestError::NotPresent))
        );
        assert_eq!(err.reason(), "handler_integrity_not_present");
    }

    #[test]
    fn integrity_mismatch_refuses_check_failed() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("h.js"), b"actual").unwrap();
        let sri = sri_sha512(b"other");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let err = prepare_checked_agent_tool_invoke(&checked(raw.as_bytes()), "search", &root)
            .err()
            .unwrap();
        assert_eq!(err.reason(), "handler_source_check_failed");
    }

    #[test]
    fn ambiguous_tool_refuses_digest_bind_failed() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"code");
        let raw = manifest_json(&format!(
            r#"[{{"id":"search","name":"A","handler":"a.js","handlerIntegrity":"{sri}"}},{{"id":"search","name":"B","handler":"b.js","handlerIntegrity":"{sri}"}}]"#
        ));
        let err = prepare_checked_agent_tool_invoke(&checked(raw.as_bytes()), "search", &root)
            .err()
            .unwrap();
        assert_eq!(err.reason(), "handler_digest_bind_failed");
    }

    #[test]
    fn unavailable_reason_constant_is_stable() {
        assert_eq!(INTEGRITY_BINDING_UNAVAILABLE_REASON, "integrity_binding_unavailable");
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
        SigningKey::from_bytes(&[13; 32])
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
    fn missing_integrity_still_not_present_when_disabled() {
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"agentTools":[{"id":"search","name":"Search","handler":"h.js"}]}}"#;
        let root = tempfile::tempfile().unwrap();
        let err = prepare_checked_agent_tool_invoke(&checked(raw), "search", &root)
            .err()
            .unwrap();
        assert_eq!(
            err,
            CheckedInvokeRefuse::Bind(HandlerSourceBindError::Digest(HandlerDigestError::NotPresent))
        );
        assert_eq!(err.reason(), "handler_integrity_not_present");
    }

    #[test]
    fn present_binding_unavailable_when_disabled_not_stub() {
        use crate::extension_js_checked_source::CheckedSourceError;
        let body = b"export const run = () => 1;";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]}}}}"#
        );
        let root = tempfile::tempfile().unwrap();
        let err = prepare_checked_agent_tool_invoke(&checked(raw.as_bytes()), "search", &root)
            .err()
            .unwrap();
        assert_eq!(
            err,
            CheckedInvokeRefuse::Bind(HandlerSourceBindError::Checked(CheckedSourceError::Unavailable))
        );
        assert_eq!(err.reason(), "handler_source_check_failed");
    }
}
