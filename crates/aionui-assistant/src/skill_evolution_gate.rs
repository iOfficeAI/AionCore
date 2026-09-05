//! Heuristic scoring gate for Skill Evolution proposals (Phase 3).
//!
//! Checklist-style signals only — advisory by default. Never injects experience
//! into Inference prompts. Enterprise auto-apply requires explicit user enablement.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    HumanOnly,
    HeuristicAssist,
    AutoApplyOnPass,
}

impl GateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HumanOnly => "human_only",
            Self::HeuristicAssist => "heuristic_assist",
            Self::AutoApplyOnPass => "auto_apply_on_pass",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "heuristic_assist" => Self::HeuristicAssist,
            "auto_apply_on_pass" => Self::AutoApplyOnPass,
            _ => Self::HumanOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRecommendation {
    Approve,
    Reject,
    NeedsReview,
}

impl GateRecommendation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::NeedsReview => "needs_review",
        }
    }

    pub fn parse(raw: Option<&str>) -> Option<Self> {
        match raw? {
            "approve" => Some(Self::Approve),
            "reject" => Some(Self::Reject),
            "needs_review" => Some(Self::NeedsReview),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateSignal {
    pub id: String,
    pub passed: bool,
    pub weight: u32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub score: u32,
    pub signals: Vec<GateSignal>,
    pub recommendation: GateRecommendation,
}

/// Detect leftover secret-looking tokens after redaction.
fn looks_secret_dirty(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for needle in [
        "sk-",
        "bearer ",
        "api_key=",
        "api-key=",
        "-----begin private key-----",
        "xoxb-",
        "ghp_",
        "github_pat_",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }
    false
}

fn extract_frontmatter(md: &str) -> Option<&str> {
    let trimmed = md.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after = &trimmed[3..];
    let end = after.find("\n---")?;
    Some(&after[..end])
}

fn body_after_frontmatter(md: &str) -> &str {
    let trimmed = md.trim_start();
    if !trimmed.starts_with("---") {
        return md.trim();
    }
    let after = &trimmed[3..];
    if let Some(end) = after.find("\n---") {
        after[end + 4..].trim()
    } else {
        md.trim()
    }
}

fn frontmatter_has_key(fm: &str, key: &str) -> bool {
    for line in fm.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix(':')
                && !v.trim().is_empty()
            {
                return true;
            }
        }
    }
    false
}

/// Score a draft SKILL.md with checklist signals (0–100).
pub fn score_draft(draft_skill_md: &str, try_run_ok: Option<bool>) -> GateResult {
    let mut signals: Vec<GateSignal> = Vec::new();
    let fm = extract_frontmatter(draft_skill_md);
    let body = body_after_frontmatter(draft_skill_md);
    let len = draft_skill_md.chars().count();

    let has_name = fm.is_some_and(|f| frontmatter_has_key(f, "name"));
    signals.push(GateSignal {
        id: "frontmatter_name".into(),
        passed: has_name,
        weight: 20,
        detail: if has_name {
            "frontmatter 含 name".into()
        } else {
            "缺少 frontmatter name".into()
        },
    });

    let has_desc = fm.is_some_and(|f| frontmatter_has_key(f, "description"));
    signals.push(GateSignal {
        id: "frontmatter_description".into(),
        passed: has_desc,
        weight: 15,
        detail: if has_desc {
            "frontmatter 含 description".into()
        } else {
            "缺少 frontmatter description".into()
        },
    });

    let has_instructions = body.chars().count() >= 40;
    signals.push(GateSignal {
        id: "non_empty_instructions".into(),
        passed: has_instructions,
        weight: 25,
        detail: if has_instructions {
            format!("正文约 {} 字", body.chars().count())
        } else {
            "正文过短或为空".into()
        },
    });

    let length_ok = (200..=80_000).contains(&len);
    signals.push(GateSignal {
        id: "length_bounds".into(),
        passed: length_ok,
        weight: 15,
        detail: format!("草案长度 {len}（建议 200–80000）"),
    });

    let secrets_clean = !looks_secret_dirty(draft_skill_md);
    signals.push(GateSignal {
        id: "secret_redaction_clean".into(),
        passed: secrets_clean,
        weight: 20,
        detail: if secrets_clean {
            "未检出明显密钥残留".into()
        } else {
            "疑似含密钥/Token，请人工复核".into()
        },
    });

    match try_run_ok {
        Some(true) => signals.push(GateSignal {
            id: "try_run".into(),
            passed: true,
            weight: 5,
            detail: "已有试跑通过信号".into(),
        }),
        Some(false) => signals.push(GateSignal {
            id: "try_run".into(),
            passed: false,
            weight: 5,
            detail: "试跑未通过或不完整".into(),
        }),
        None => signals.push(GateSignal {
            id: "try_run".into(),
            passed: true, // optional — do not punish absence
            weight: 0,
            detail: "无试跑结果（可选）".into(),
        }),
    }

    let total_weight: u32 = signals.iter().map(|s| s.weight).sum::<u32>().max(1);
    let earned: u32 = signals.iter().filter(|s| s.passed).map(|s| s.weight).sum();
    let score = ((earned as u64 * 100) / total_weight as u64) as u32;

    let recommendation = if !secrets_clean || score < 40 {
        GateRecommendation::Reject
    } else if score >= 80 && has_name && has_desc && has_instructions {
        GateRecommendation::Approve
    } else {
        GateRecommendation::NeedsReview
    };

    GateResult {
        score,
        signals,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_stub_scores_high() {
        let md = "---\nname: evolved-demo\ndescription: demo skill\nversion: 0.1.0\n---\n\n# Demo\n\n## 使用指引\n\n1. 确认适用范围。\n2. 在智能体中心试跑。\n3. 发布时 pin。\n";
        let r = score_draft(md, None);
        assert!(r.score >= 80, "score={}", r.score);
        assert_eq!(r.recommendation, GateRecommendation::Approve);
    }

    #[test]
    fn empty_draft_scores_low() {
        let r = score_draft("", None);
        assert!(r.score < 40);
        assert_eq!(r.recommendation, GateRecommendation::Reject);
    }

    #[test]
    fn secret_triggers_reject() {
        let md = "---\nname: x\ndescription: y\n---\n\nUse key sk-abcdefghijklmnopqrstuvwxyz123456\n\nand more instructions here for length.\n";
        let r = score_draft(md, None);
        assert_eq!(r.recommendation, GateRecommendation::Reject);
        assert!(
            !r.signals
                .iter()
                .find(|s| s.id == "secret_redaction_clean")
                .unwrap()
                .passed
        );
    }
}
