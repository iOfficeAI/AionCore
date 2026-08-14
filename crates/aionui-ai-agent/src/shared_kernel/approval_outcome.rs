//! Closed approval outcomes, modeled on DeepSeek Harness `ApprovalOutcome`.
//!
//! The only grant is `allowed-once`. Missing, unparseable, or unattended
//! answers fail closed. `AllowAlways` is a host overlay after a grant, not a
//! fourth outcome.

/// Closed approval outcomes. Callers fail closed on anything except `AllowedOnce`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }

    /// Only an explicit one-shot grant authorizes the asked operation.
    pub fn grants(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }
}

/// Session approval policy applied *before* any interactive answerer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Delegate to the answerer. No answerer → `Unavailable`.
    Ask,
    /// Never prompt: every ask is `Rejected`.
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// What kind of pending ask the host is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAskKind {
    /// No live pending request. The question was withdrawn or never arrived.
    Missing,
    /// Ordinary tool permission card (`allow` / `allow_always` / `reject`).
    Permission,
    /// Structured question whose labels ride the confirm path.
    AskUserQuestion,
}

const PERM_ALLOW: &str = "allow";
const PERM_ALLOW_ALWAYS: &str = "allow_always";
const PERM_REJECT: &str = "reject";

/// Resolve one permission answer into a closed outcome.
///
/// `never` rejects before looking at the picked option. A missing pending
/// request is `cancelled`. Unknown or empty picks on a permission card are
/// `unavailable`. AskUserQuestion still accepts a non-empty label as a grant
/// so the dedicated question path's labels keep working.
pub fn resolve_approval_outcome(
    policy: ApprovalPolicy,
    picked: Option<&str>,
    always_allow: bool,
    ask_kind: ApprovalAskKind,
) -> ApprovalOutcome {
    if matches!(policy, ApprovalPolicy::Never) {
        return ApprovalOutcome::Rejected;
    }
    if matches!(ask_kind, ApprovalAskKind::Missing) {
        return ApprovalOutcome::Cancelled;
    }
    if always_allow {
        return ApprovalOutcome::AllowedOnce;
    }
    match picked {
        Some(PERM_REJECT) => ApprovalOutcome::Rejected,
        Some(PERM_ALLOW) | Some(PERM_ALLOW_ALWAYS) => ApprovalOutcome::AllowedOnce,
        Some(label) if !label.is_empty() && matches!(ask_kind, ApprovalAskKind::AskUserQuestion) => {
            ApprovalOutcome::AllowedOnce
        }
        Some(_) | None => ApprovalOutcome::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_policy_rejects_even_with_an_allow() {
        let outcome = resolve_approval_outcome(ApprovalPolicy::Ask, Some("allow"), false, ApprovalAskKind::Permission);
        assert!(outcome.grants());
        let denied = resolve_approval_outcome(ApprovalPolicy::Never, Some("allow"), true, ApprovalAskKind::Permission);
        assert_eq!(denied, ApprovalOutcome::Rejected);
        assert!(!denied.grants());
    }

    #[test]
    fn missing_pending_request_is_cancelled() {
        let outcome = resolve_approval_outcome(ApprovalPolicy::Ask, Some("allow"), false, ApprovalAskKind::Missing);
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
        assert!(!outcome.grants());
    }

    #[test]
    fn missing_or_unknown_permission_pick_is_unavailable() {
        assert_eq!(
            resolve_approval_outcome(ApprovalPolicy::Ask, None, false, ApprovalAskKind::Permission),
            ApprovalOutcome::Unavailable
        );
        assert_eq!(
            resolve_approval_outcome(
                ApprovalPolicy::Ask,
                Some("not-a-real-option"),
                false,
                ApprovalAskKind::Permission
            ),
            ApprovalOutcome::Unavailable
        );
    }

    #[test]
    fn ask_user_question_label_still_grants() {
        let outcome = resolve_approval_outcome(
            ApprovalPolicy::Ask,
            Some("Ship it"),
            false,
            ApprovalAskKind::AskUserQuestion,
        );
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
    }

    #[test]
    fn empty_ask_label_is_unavailable() {
        assert_eq!(
            resolve_approval_outcome(ApprovalPolicy::Ask, Some(""), false, ApprovalAskKind::AskUserQuestion),
            ApprovalOutcome::Unavailable
        );
    }
}
