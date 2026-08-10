use serde::Deserialize;

/// Body for `POST /api/conversations/:id/asks/:requestId/answer` — the
/// structured-question card's DEDICATED answer channel (2026-08-05 ruling:
/// question answers must not ride the permission confirm endpoint).
///
/// Exactly one of the two shapes is valid:
/// - answered: `answers` non-empty, `decline` false/absent
/// - dismissed: `decline: true`, `answers` absent
///
/// A decline MUST stay distinguishable from an empty answer set — claude
/// silently drops unanswered questions on an allow (live 2.1.178), so an
/// "empty allow" is silent data loss, not a re-ask.
#[derive(Debug, Deserialize)]
pub struct AskAnswerRequest {
    #[serde(default)]
    pub answers: Vec<AskQuestionAnswer>,
    #[serde(default)]
    pub decline: bool,
}

/// One answered question: `question` is the exact question TEXT (claude keys
/// its answers map by it), `labels` the chosen option labels (one for a
/// single-select, one-or-more for multiSelect; free text rides as a label).
#[derive(Debug, Deserialize)]
pub struct AskQuestionAnswer {
    pub question: String,
    pub labels: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_answered() {
        let req: AskAnswerRequest = serde_json::from_value(json!({
            "answers": [{ "question": "Which?", "labels": ["A", "B"] }]
        }))
        .unwrap();
        assert!(!req.decline);
        assert_eq!(req.answers.len(), 1);
        assert_eq!(req.answers[0].labels, vec!["A", "B"]);
    }

    #[test]
    fn deserialize_decline() {
        let req: AskAnswerRequest = serde_json::from_value(json!({ "decline": true })).unwrap();
        assert!(req.decline);
        assert!(req.answers.is_empty());
    }
}
