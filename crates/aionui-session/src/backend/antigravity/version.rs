//! Version drift detection for the `agy` CLI.
//!
//! agy is NOT shipped with AionUi — it is a 157 MB binary Google distributes
//! itself, so unlike claude/codex (public npm packages pinned into
//! `managed-resources`) its version is whatever the user installed. Every wire
//! contract this backend relies on — the `stream-json` event shapes, the
//! `PreToolUse` hook protocol, `--conversation` resume — was verified against
//! one version. A drifting install must therefore be visible rather than
//! failing in some unexplained way mid-turn.

use crate::event::{LocalizedText, NoticeLevel};

/// The agy release every wire contract here was verified against.
///
/// Bumping this means re-verifying against captured traffic, not just editing
/// the constant.
pub(crate) const VERIFIED_AGY_VERSION: &str = "1.1.9";

/// How the installed agy compares to the one this backend was built against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionVerdict {
    /// Exactly the verified release.
    Verified,
    /// Older than verified — features this backend uses may be missing.
    Older,
    /// Newer than verified — the wire contracts may have moved.
    Newer,
    /// Unparseable output; nothing can be concluded, so nothing is claimed.
    Unknown,
}

/// `agy --version` prints a bare `1.1.9`. Be liberal anyway: take the first
/// dotted numeric run so a future `agy version 1.2.0 (abc123)` still parses.
fn parse_version(raw: &str) -> Option<Vec<u32>> {
    let token = raw.split_whitespace().find(|t| {
        let core = t.trim_start_matches('v');
        core.chars().next().is_some_and(|c| c.is_ascii_digit()) && core.contains('.')
    })?;
    let parts: Vec<u32> = token
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()))
        .map(|p| p.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

pub(crate) fn classify_version(reported: &str) -> VersionVerdict {
    let (Some(actual), Some(verified)) = (parse_version(reported), parse_version(VERIFIED_AGY_VERSION)) else {
        return VersionVerdict::Unknown;
    };
    match actual.cmp(&verified) {
        std::cmp::Ordering::Equal => VersionVerdict::Verified,
        std::cmp::Ordering::Less => VersionVerdict::Older,
        std::cmp::Ordering::Greater => VersionVerdict::Newer,
    }
}

/// i18n key for the "some steps in this turn did not take effect" notice,
/// interpolating `{{count}}`.
pub(crate) const CODE_STEPS_FAILED: &str = "ANTIGRAVITY_STEPS_FAILED";

/// i18n keys for the two drift verdicts, both interpolating
/// `{{reported}}` / `{{verified}}`. All three resolve on the frontend under
/// `conversation.agentTip.codes.<code>.body`.
pub(crate) const CODE_VERSION_OLDER: &str = "ANTIGRAVITY_VERSION_OLDER";
pub(crate) const CODE_VERSION_NEWER: &str = "ANTIGRAVITY_VERSION_NEWER";

/// The user-facing warning for a drifting install, or `None` when there is
/// nothing worth saying.
///
/// A NEWER agy is not treated as broken: it usually works, and blocking it
/// would strand users on an old release. It is reported once so that, if the
/// session then misbehaves, the cause is already on screen.
///
/// Returns the English text AND its translation handle: the text is the
/// fallback shown when the locale has no entry for the code, so both travel
/// together rather than the caller having to rebuild one from the other.
pub(crate) fn drift_notice(reported: &str) -> Option<(NoticeLevel, String, LocalizedText)> {
    let localized = |code: &str| {
        LocalizedText::new(code)
            .with("reported", reported)
            .with("verified", VERIFIED_AGY_VERSION)
    };
    match classify_version(reported) {
        VersionVerdict::Verified => None,
        VersionVerdict::Unknown => None,
        VersionVerdict::Older => Some((
            NoticeLevel::Warning,
            format!(
                "agy {reported} is older than the {VERIFIED_AGY_VERSION} this integration was verified against; \
                 some features may be missing. Consider upgrading agy."
            ),
            localized(CODE_VERSION_OLDER),
        )),
        VersionVerdict::Newer => Some((
            NoticeLevel::Info,
            format!(
                "agy {reported} is newer than the {VERIFIED_AGY_VERSION} this integration was verified against. \
                 It should still work; report anything that behaves oddly."
            ),
            localized(CODE_VERSION_NEWER),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verified_release_says_nothing() {
        assert_eq!(classify_version("1.1.9"), VersionVerdict::Verified);
        assert!(drift_notice("1.1.9").is_none());
    }

    #[test]
    fn an_older_install_warns_because_features_may_be_missing() {
        assert_eq!(classify_version("1.1.8"), VersionVerdict::Older);
        let (level, msg, loc) = drift_notice("1.1.8").expect("older must be reported");
        assert_eq!(level, NoticeLevel::Warning);
        assert!(msg.contains("1.1.8") && msg.contains(VERIFIED_AGY_VERSION));
        // The English text is the fallback; the code+params are what a
        // translated build actually renders, so both must be present.
        assert_eq!(loc.code, CODE_VERSION_OLDER);
        assert_eq!(loc.params.get("reported").and_then(|v| v.as_str()), Some("1.1.8"));
        assert_eq!(
            loc.params.get("verified").and_then(|v| v.as_str()),
            Some(VERIFIED_AGY_VERSION)
        );
    }

    #[test]
    fn a_newer_install_informs_but_does_not_block() {
        // Blocking would strand users on an old agy every time Google ships one.
        assert_eq!(classify_version("1.2.0"), VersionVerdict::Newer);
        let (level, _, loc) = drift_notice("1.2.0").expect("newer must be reported");
        assert_eq!(level, NoticeLevel::Info);
        assert_eq!(loc.code, CODE_VERSION_NEWER);
    }

    #[test]
    fn unparseable_output_claims_nothing() {
        // Asserting drift from output we could not read would be a fabrication.
        for raw in ["", "unknown", "error: not signed in"] {
            assert_eq!(classify_version(raw), VersionVerdict::Unknown, "raw={raw:?}");
            assert!(drift_notice(raw).is_none());
        }
    }

    #[test]
    fn a_decorated_version_line_still_parses() {
        assert_eq!(
            classify_version("agy version v1.1.9 (build abc)"),
            VersionVerdict::Verified
        );
        assert_eq!(classify_version("1.1.10"), VersionVerdict::Newer);
    }

    #[test]
    fn shorter_and_longer_version_tuples_compare_sanely() {
        assert_eq!(classify_version("1.2"), VersionVerdict::Newer);
        assert_eq!(classify_version("1.1"), VersionVerdict::Older);
        assert_eq!(classify_version("1.1.9.1"), VersionVerdict::Newer);
    }
}

/// Drift guidance for an installed agy, as plain advisory text — no `Notice`,
/// no level.
///
/// The availability probe already runs `agy --version` for its integrity check
/// and reaches the user BEFORE any conversation exists, which is where a
/// version warning actually helps: the user is choosing whether to rely on this
/// agent. It has no notice channel, only a guidance string, so the same verdict
/// is exposed here in the shape that path can use.
pub fn version_guidance(reported: &str) -> Option<String> {
    drift_notice(reported).map(|(_, message, _)| message)
}

#[cfg(test)]
mod guidance_tests {
    use super::*;

    #[test]
    fn a_matching_install_gets_no_guidance() {
        // Silence is the whole point: an ⓘ on every connection test would be
        // noise, and the verified version is what most users have.
        assert_eq!(version_guidance(VERIFIED_AGY_VERSION), None);
    }

    #[test]
    fn a_drifting_install_gets_actionable_text() {
        let older = version_guidance("1.1.8").expect("older must be reported");
        assert!(older.contains("1.1.8") && older.contains(VERIFIED_AGY_VERSION));
        let newer = version_guidance("1.2.0").expect("newer must be reported");
        assert!(newer.contains("1.2.0"));
    }

    #[test]
    fn unreadable_version_output_says_nothing() {
        // The probe hands over whatever the CLI printed. Claiming drift from a
        // line we could not parse would invent a problem.
        for raw in ["", "error: not signed in", "???"] {
            assert_eq!(version_guidance(raw), None, "raw={raw:?}");
        }
    }
}
