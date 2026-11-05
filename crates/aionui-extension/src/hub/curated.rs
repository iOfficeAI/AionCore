//! Enterprise curated marketplace policy (M9 E13 / S10).
//!
//! Brand: CSBU WorkMate. Contract v1 installs are curated/allowlist only
//! (D13-1). Optional ed25519 signatures when pubkeys are configured.
//!
//! Env:
//! - `WORKMATE_HUB_ALLOWLIST` — comma-separated extension names; when set,
//!   only listed names (plus bundled) may install.
//! - `WORKMATE_HUB_SIGNING_PUBKEYS` — JSON object `{ "key-id": "<base64 pk>" }`
//!   of ed25519 verifying keys. When an index entry carries `signature` +
//!   `pubkey_id`, verification is required against this map.
//! - `WORKMATE_HUB_REQUIRE_SIGNATURE` — when `1`/`true`, non-bundled curated
//!   entries must carry a verifiable signature.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha512};
use tracing::warn;

use super::index_manager::HubIndexEntry;

/// Env: comma-separated allowlist of extension names.
pub const ALLOWLIST_ENV: &str = "WORKMATE_HUB_ALLOWLIST";
/// Env: JSON map of pubkey_id → base64-encoded ed25519 verifying key (32 bytes).
pub const SIGNING_PUBKEYS_ENV: &str = "WORKMATE_HUB_SIGNING_PUBKEYS";
/// Env: require signatures on non-bundled curated installs.
pub const REQUIRE_SIGNATURE_ENV: &str = "WORKMATE_HUB_REQUIRE_SIGNATURE";

/// Canonical payload signed for a hub entry: `name\nversion\nintegrity`.
pub fn signature_payload(name: &str, version: &str, integrity: &str) -> String {
    format!("{name}\n{version}\n{integrity}")
}

/// Compute SRI `sha512-<base64>` over raw bytes.
pub fn sri_sha512(bytes: &[u8]) -> String {
    let digest = Sha512::digest(bytes);
    format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(digest))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CuratedGateError {
    NotInIndex,
    NotCurated { name: String },
    NotAllowlisted { name: String },
    IntegrityMissing { name: String },
    IntegrityMismatch { name: String },
    SignatureRequired { name: String },
    SignatureMissingPubkey { name: String },
    SignatureInvalid { name: String },
    PubkeyUnknown { name: String, pubkey_id: String },
    PubkeyConfig(String),
}

impl std::fmt::Display for CuratedGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInIndex => write!(f, "extension not found in hub index"),
            Self::NotCurated { name } => write!(
                f,
                "extension '{name}' is not curated; CSBU WorkMate v1 only installs curated or bundled plugins"
            ),
            Self::NotAllowlisted { name } => write!(f, "extension '{name}' is not on WORKMATE_HUB_ALLOWLIST"),
            Self::IntegrityMissing { name } => {
                write!(f, "extension '{name}' is missing integrity (SRI sha512)")
            }
            Self::IntegrityMismatch { name } => {
                write!(f, "extension '{name}' failed integrity verification")
            }
            Self::SignatureRequired { name } => {
                write!(f, "extension '{name}' requires an ed25519 signature")
            }
            Self::SignatureMissingPubkey { name } => {
                write!(f, "extension '{name}' signature lacks pubkey_id")
            }
            Self::SignatureInvalid { name } => {
                write!(f, "extension '{name}' signature verification failed")
            }
            Self::PubkeyUnknown { name, pubkey_id } => {
                write!(f, "extension '{name}' signed by unknown pubkey_id '{pubkey_id}'")
            }
            Self::PubkeyConfig(msg) => write!(f, "signing pubkey config error: {msg}"),
        }
    }
}

fn parse_allowlist(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

fn load_pubkeys_from_env() -> Result<HashMap<String, VerifyingKey>, CuratedGateError> {
    let Ok(raw) = std::env::var(SIGNING_PUBKEYS_ENV) else {
        return Ok(HashMap::new());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let map: HashMap<String, String> =
        serde_json::from_str(trimmed).map_err(|e| CuratedGateError::PubkeyConfig(e.to_string()))?;
    let mut out = HashMap::new();
    for (id, b64) in map {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| CuratedGateError::PubkeyConfig(format!("pubkey {id}: {e}")))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| CuratedGateError::PubkeyConfig(format!("pubkey {id}: expected 32 bytes")))?;
        let key =
            VerifyingKey::from_bytes(&arr).map_err(|e| CuratedGateError::PubkeyConfig(format!("pubkey {id}: {e}")))?;
        out.insert(id, key);
    }
    Ok(out)
}

/// Cached pubkeys from env (process lifetime). Tests should set env before first call
/// or use [`evaluate_install_with`] with explicit keys.
fn cached_pubkeys() -> Result<&'static HashMap<String, VerifyingKey>, CuratedGateError> {
    static KEYS: OnceLock<Result<HashMap<String, VerifyingKey>, String>> = OnceLock::new();
    let slot = KEYS.get_or_init(|| load_pubkeys_from_env().map_err(|e| e.to_string()));
    match slot {
        Ok(map) => Ok(map),
        Err(msg) => Err(CuratedGateError::PubkeyConfig(msg.clone())),
    }
}

/// Policy snapshot used by the install gate.
#[derive(Debug, Clone)]
pub struct CuratedInstallPolicy {
    pub allowlist: HashSet<String>,
    pub require_signature: bool,
    pub pubkeys: HashMap<String, VerifyingKey>,
}

impl CuratedInstallPolicy {
    /// Load from process environment.
    pub fn from_env() -> Result<Self, CuratedGateError> {
        let allowlist = std::env::var(ALLOWLIST_ENV)
            .ok()
            .map(|s| parse_allowlist(&s))
            .unwrap_or_default();
        let require_signature = env_truthy(REQUIRE_SIGNATURE_ENV);
        let pubkeys = cached_pubkeys()?.clone();
        Ok(Self {
            allowlist,
            require_signature,
            pubkeys,
        })
    }

    /// Test/helper constructor (bypasses env cache).
    pub fn new(allowlist: HashSet<String>, require_signature: bool, pubkeys: HashMap<String, VerifyingKey>) -> Self {
        Self {
            allowlist,
            require_signature,
            pubkeys,
        }
    }
}

/// Evaluate whether `entry` may be installed. `manifest_bytes` is the on-disk
/// `aion-extension.json` used for integrity when `entry.integrity` is set.
pub(crate) fn evaluate_install(entry: &HubIndexEntry, manifest_bytes: Option<&[u8]>) -> Result<(), CuratedGateError> {
    let policy = CuratedInstallPolicy::from_env()?;
    evaluate_install_with(&policy, entry, manifest_bytes)
}

pub(crate) fn evaluate_install_with(
    policy: &CuratedInstallPolicy,
    entry: &HubIndexEntry,
    manifest_bytes: Option<&[u8]>,
) -> Result<(), CuratedGateError> {
    // Bundled extensions are always installable (shipped with the product).
    if entry.bundled {
        return Ok(());
    }

    if !entry.curated {
        return Err(CuratedGateError::NotCurated {
            name: entry.name.clone(),
        });
    }

    if !policy.allowlist.is_empty() && !policy.allowlist.contains(&entry.name) {
        return Err(CuratedGateError::NotAllowlisted {
            name: entry.name.clone(),
        });
    }

    // Integrity: when declared, must match sha512 of manifest bytes.
    if let Some(expected) = entry.integrity.as_deref() {
        let Some(bytes) = manifest_bytes else {
            return Err(CuratedGateError::IntegrityMissing {
                name: entry.name.clone(),
            });
        };
        let actual = sri_sha512(bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(CuratedGateError::IntegrityMismatch {
                name: entry.name.clone(),
            });
        }
    } else if policy.require_signature {
        // Signature requirement implies integrity must be present to bind the payload.
        return Err(CuratedGateError::IntegrityMissing {
            name: entry.name.clone(),
        });
    }

    let has_sig = entry.signature.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);

    if policy.require_signature && !has_sig {
        return Err(CuratedGateError::SignatureRequired {
            name: entry.name.clone(),
        });
    }

    if has_sig {
        let pubkey_id = entry.pubkey_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
            CuratedGateError::SignatureMissingPubkey {
                name: entry.name.clone(),
            }
        })?;
        let Some(key) = policy.pubkeys.get(pubkey_id) else {
            // If no pubkeys configured at all and signature not required, warn + skip.
            if policy.pubkeys.is_empty() && !policy.require_signature {
                warn!(
                    name = %entry.name,
                    pubkey_id,
                    "hub: signature present but WORKMATE_HUB_SIGNING_PUBKEYS empty; skipping verify"
                );
                return Ok(());
            }
            return Err(CuratedGateError::PubkeyUnknown {
                name: entry.name.clone(),
                pubkey_id: pubkey_id.to_owned(),
            });
        };
        let integrity = entry.integrity.as_deref().unwrap_or("");
        let payload = signature_payload(&entry.name, &entry.version, integrity);
        let sig_bytes = hex::decode(entry.signature.as_deref().unwrap_or("").trim()).map_err(|_| {
            CuratedGateError::SignatureInvalid {
                name: entry.name.clone(),
            }
        })?;
        let signature = Signature::from_slice(&sig_bytes).map_err(|_| CuratedGateError::SignatureInvalid {
            name: entry.name.clone(),
        })?;
        key.verify(payload.as_bytes(), &signature)
            .map_err(|_| CuratedGateError::SignatureInvalid {
                name: entry.name.clone(),
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn entry(curated: bool, bundled: bool) -> HubIndexEntry {
        HubIndexEntry {
            name: "demo.plugin".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: vec![],
            bundled,
            download_url: None,
            curated,
            integrity: None,
            signature: None,
            pubkey_id: None,
        }
    }

    #[test]
    fn bundled_bypasses_curated() {
        let policy = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new());
        assert!(evaluate_install_with(&policy, &entry(false, true), None).is_ok());
    }

    #[test]
    fn non_curated_rejected() {
        let policy = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new());
        let err = evaluate_install_with(&policy, &entry(false, false), None).unwrap_err();
        assert!(matches!(err, CuratedGateError::NotCurated { .. }));
    }

    #[test]
    fn allowlist_blocks_unknown() {
        let policy = CuratedInstallPolicy::new(HashSet::from(["other".into()]), false, HashMap::new());
        let err = evaluate_install_with(&policy, &entry(true, false), None).unwrap_err();
        assert!(matches!(err, CuratedGateError::NotAllowlisted { .. }));
    }

    #[test]
    fn integrity_and_signature_roundtrip() {
        let seed = [7u8; 32];
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        let manifest = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let integrity = sri_sha512(manifest);
        let payload = signature_payload("demo.plugin", "1.0.0", &integrity);
        let sig = signing.sign(payload.as_bytes());
        let mut e = entry(true, false);
        e.integrity = Some(integrity);
        e.signature = Some(hex::encode(sig.to_bytes()));
        e.pubkey_id = Some("enterprise".into());
        let policy = CuratedInstallPolicy::new(HashSet::new(), true, HashMap::from([("enterprise".into(), verifying)]));
        assert!(evaluate_install_with(&policy, &e, Some(manifest)).is_ok());
    }
}
