//! Per-vendor skill delivery declaration (`agent_metadata.skill_delivery`).
//!
//! This column is an OPEN extension point, not a closed state machine: shipping
//! a new vendor capability must be a data change, never a migration + release.
//! That is why the DB carries no CHECK constraint and why parsing here is
//! deliberately tolerant — an unknown `mode` means "a newer registry wrote
//! this", which is expected and must degrade to `injected`, NOT fail the row.

use serde::{Deserialize, Serialize};

/// How a vendor receives the conversation's skills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeliveryMode {
    /// Launch-argument delivery (e.g. claude / codebuddy `--plugin-dir`).
    Argv,
    /// Protocol-request delivery (e.g. codex `skills/extraRoots/set`).
    Protocol,
    /// Prompt injection + dual channel. The safe default.
    Injected,
}

/// Raw shape as stored. `mode` stays a `String` here so an unrecognized value
/// cannot fail the whole deserialization.
#[derive(Debug, Clone, Deserialize)]
struct RawSkillDelivery {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    allow_dir_args: Vec<String>,
    #[serde(default)]
    method: Option<String>,
}

/// Decoded delivery config. `mode` is always one of the three known values —
/// an unknown input is reported separately by [`SkillDeliveryParse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDelivery {
    pub mode: SkillDeliveryMode,
    #[serde(default)]
    pub args: Vec<String>,
    /// Cross-cutting A: directories to hand the CLI as readable. Independent of
    /// `mode` — spec §10.2 #9 proved `--plugin-dir` does NOT exempt a skill
    /// directory from the CLI's file-permission check, so layer 1 needs it too.
    #[serde(default)]
    pub allow_dir_args: Vec<String>,
    #[serde(default)]
    pub method: Option<String>,
}

impl SkillDelivery {
    /// The safe default: no vendor capability claimed, dual-channel covers it.
    pub fn injected_default() -> Self {
        Self {
            mode: SkillDeliveryMode::Injected,
            args: Vec::new(),
            allow_dir_args: Vec::new(),
            method: None,
        }
    }
}

/// Why three branches and not `Result`: "a newer registry wrote a mode we don't
/// know" and "this JSON is corrupt" need DIFFERENT log lines. Collapsing them
/// into one "parse failed" makes registry rollout problems undiagnosable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDeliveryParse {
    Ok(SkillDelivery),
    UnknownMode { raw_mode: String, delivery: SkillDelivery },
    Malformed { error: String },
}

pub fn parse_skill_delivery(raw: Option<&str>) -> SkillDeliveryParse {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return SkillDeliveryParse::Ok(SkillDelivery::injected_default());
    };
    let parsed: RawSkillDelivery = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            return SkillDeliveryParse::Malformed {
                error: error.to_string(),
            };
        }
    };

    let mode_text = parsed.mode.as_deref().map(str::trim).unwrap_or("");
    let mode = match mode_text {
        "" | "injected" => Some(SkillDeliveryMode::Injected),
        "argv" => Some(SkillDeliveryMode::Argv),
        "protocol" => Some(SkillDeliveryMode::Protocol),
        _ => None,
    };

    let delivery = SkillDelivery {
        // Unknown mode degrades to injected while KEEPING args/allow_dir_args:
        // `allow_dir_args` is orthogonal to mode and still correct.
        mode: mode.clone().unwrap_or(SkillDeliveryMode::Injected),
        args: parsed.args,
        allow_dir_args: parsed.allow_dir_args,
        method: parsed.method,
    };
    match mode {
        Some(_) => SkillDeliveryParse::Ok(delivery),
        None => SkillDeliveryParse::UnknownMode {
            raw_mode: mode_text.to_owned(),
            delivery,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_falls_back_to_injected() {
        let parsed = parse_skill_delivery(None);
        let SkillDeliveryParse::Ok(delivery) = parsed else {
            panic!("NULL must parse as a plain injected default");
        };
        assert_eq!(delivery.mode, SkillDeliveryMode::Injected);
        assert!(delivery.args.is_empty());
        assert!(delivery.allow_dir_args.is_empty());
    }

    #[test]
    fn argv_mode_keeps_args_and_allow_dir_args() {
        let raw = r#"{"mode":"argv","args":["--plugin-dir","{skill_view_dir}"],
                      "allow_dir_args":["--add-dir","{skill_dir}"]}"#;
        let SkillDeliveryParse::Ok(delivery) = parse_skill_delivery(Some(raw)) else {
            panic!("a well-formed argv config must parse cleanly");
        };
        assert_eq!(delivery.mode, SkillDeliveryMode::Argv);
        assert_eq!(delivery.args, vec!["--plugin-dir", "{skill_view_dir}"]);
        assert_eq!(delivery.allow_dir_args, vec!["--add-dir", "{skill_dir}"]);
    }

    #[test]
    fn protocol_mode_keeps_method() {
        let raw = r#"{"mode":"protocol","method":"skills/extraRoots/set"}"#;
        let SkillDeliveryParse::Ok(delivery) = parse_skill_delivery(Some(raw)) else {
            panic!("a well-formed protocol config must parse cleanly");
        };
        assert_eq!(delivery.mode, SkillDeliveryMode::Protocol);
        assert_eq!(delivery.method.as_deref(), Some("skills/extraRoots/set"));
    }

    /// A registry newer than this binary is EXPECTED, not corruption: the whole
    /// point of the column is shipping new vendor capabilities as data.
    #[test]
    fn an_unknown_mode_reports_the_actual_value_and_falls_back() {
        let raw = r#"{"mode":"future_mode_v9","allow_dir_args":["--add-dir","{skill_dir}"]}"#;
        let SkillDeliveryParse::UnknownMode { raw_mode, delivery } = parse_skill_delivery(Some(raw)) else {
            panic!("an unknown mode must be its own branch, not Malformed and not Ok");
        };
        assert_eq!(raw_mode, "future_mode_v9");
        assert_eq!(delivery.mode, SkillDeliveryMode::Injected, "fall back, never fail");
        assert_eq!(
            delivery.allow_dir_args,
            vec!["--add-dir", "{skill_dir}"],
            "allow_dir_args is independent of mode and must survive the fallback"
        );
    }

    #[test]
    fn malformed_json_is_a_distinct_branch_from_an_unknown_mode() {
        let SkillDeliveryParse::Malformed { error } = parse_skill_delivery(Some("{not json")) else {
            panic!("corrupt data must be distinguishable from a newer registry");
        };
        assert!(!error.is_empty(), "the error text is what makes it diagnosable");
    }

    #[test]
    fn a_missing_mode_key_is_injected_not_malformed() {
        let SkillDeliveryParse::Ok(delivery) = parse_skill_delivery(Some(r#"{"args":[]}"#)) else {
            panic!("an absent mode is the safe default, not an error");
        };
        assert_eq!(delivery.mode, SkillDeliveryMode::Injected);
    }
}
