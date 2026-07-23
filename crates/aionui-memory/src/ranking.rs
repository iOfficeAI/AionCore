use aionui_db::models::MemoryEntryRow;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankingContext {
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub current_conversation_id: String,
    pub reset_at: Option<i64>,
    pub now: i64,
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedSelection {
    pub entries: Vec<MemoryEntryRow>,
    pub estimated_tokens: u32,
}

pub(crate) fn estimate_tokens(text: &str) -> u32 {
    let (ascii, non_ascii) = text.chars().fold((0_u32, 0_u32), |(ascii, non_ascii), character| {
        if character.is_ascii() {
            (ascii.saturating_add(1), non_ascii)
        } else {
            (ascii, non_ascii.saturating_add(1))
        }
    });
    ascii.div_ceil(3).saturating_add(non_ascii)
}

pub(crate) fn retrieval_budget(capacity: Option<u32>) -> u32 {
    capacity.map_or(2_000, |capacity| capacity / 10).min(2_000)
}

pub(crate) fn select_entries(
    prompt: &str,
    candidates: Vec<MemoryEntryRow>,
    context: &RankingContext,
) -> RankedSelection {
    let prompt_tokens = normalized_tokens(prompt);
    let mut scored = candidates
        .into_iter()
        .filter_map(|entry| score_entry(entry, &prompt_tokens, context))
        .collect::<Vec<_>>();
    scored.sort_by_key(|entry| {
        (
            entry.scope_rank,
            Reverse(entry.score),
            Reverse(entry.source_count),
            Reverse(entry.updated_at),
            entry.entry.id.clone(),
        )
    });

    let mut estimated_tokens = 0_u32;
    let mut entries = Vec::new();
    for scored in scored {
        let Some(content) = scored.entry.content.as_deref() else {
            continue;
        };
        let tokens = estimate_tokens(content);
        if tokens == 0 || estimated_tokens.saturating_add(tokens) > context.budget_tokens {
            continue;
        }
        estimated_tokens += tokens;
        entries.push(scored.entry);
    }
    RankedSelection {
        entries,
        estimated_tokens,
    }
}

#[derive(Debug)]
struct ScoredEntry {
    entry: MemoryEntryRow,
    scope_rank: u8,
    score: i64,
    source_count: usize,
    updated_at: i64,
}

fn score_entry(
    mut entry: MemoryEntryRow,
    prompt_tokens: &BTreeSet<String>,
    context: &RankingContext,
) -> Option<ScoredEntry> {
    if entry.state != "active" || entry.content.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    if let Some(reset_at) = context.reset_at {
        entry.sources.retain(|source| source.last_observed_at >= reset_at);
    }
    if entry.sources.is_empty() {
        return None;
    }
    if entry
        .sources
        .iter()
        .all(|source| source.conversation_id == context.current_conversation_id)
    {
        return None;
    }

    let project_match = context.project_id.is_some() && entry.project_id == context.project_id;
    let workspace_match = context.workspace_key.is_some() && entry.workspace_key == context.workspace_key;
    let global = entry.project_id.is_none() && entry.workspace_key.is_none();
    let scope_rank = if project_match && workspace_match {
        0
    } else if project_match || workspace_match {
        1
    } else if global {
        2
    } else {
        return None;
    };

    let mut searchable = entry.content.as_deref().unwrap_or_default().to_owned();
    searchable.push(' ');
    searchable.push_str(&entry.stable_key);
    let entry_tokens = normalized_tokens(&searchable);
    let relevance = prompt_tokens.intersection(&entry_tokens).count() as i64;
    if global && relevance == 0 {
        return None;
    }

    let mut score = relevance * 100;
    if entry.pinned {
        score += 10_000;
    }
    if entry.user_edited {
        score += 8_000;
    }
    score += kind_weight(&entry.kind);
    if !entry.pinned && !entry.user_edited {
        let age_days = context.now.saturating_sub(entry.updated_at).max(0) / 86_400_000;
        score -= age_days.min(365);
    }
    let source_count = entry
        .sources
        .iter()
        .map(|source| source.conversation_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let updated_at = entry.updated_at;
    Some(ScoredEntry {
        entry,
        scope_rank,
        score,
        source_count,
        updated_at,
    })
}

fn normalized_tokens(text: &str) -> BTreeSet<String> {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

fn kind_weight(kind: &str) -> i64 {
    match kind {
        "decision" => 60,
        "work_constraint" => 50,
        "next_step" => 40,
        "outcome" => 30,
        "artifact" => 20,
        "issue" => 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use aionui_db::models::{MemoryEntryRow, MemorySourceRow};

    use super::{RankingContext, estimate_tokens, retrieval_budget, select_entries};

    fn source(entry: &str, conversation: &str) -> MemorySourceRow {
        MemorySourceRow {
            memory_entry_id: entry.into(),
            conversation_id: conversation.into(),
            turn_id: format!("turn-{conversation}"),
            message_ids_json: "[]".into(),
            first_observed_at: 1_000,
            last_observed_at: 1_000,
        }
    }

    fn entry(id: &str, content: &str) -> MemoryEntryRow {
        MemoryEntryRow {
            id: id.into(),
            user_id: "user-1".into(),
            project_id: None,
            workspace_key: None,
            kind: "issue".into(),
            stable_key: id.into(),
            fingerprint: format!("fp-{id}"),
            content: Some(content.into()),
            state: "active".into(),
            pinned: false,
            user_edited: false,
            revision: 1,
            supersedes_id: None,
            conflict_group_id: None,
            schema_version: 1,
            deleted_at: None,
            created_at: 1,
            updated_at: 1_000,
            sources: vec![source(id, "source-1")],
        }
    }

    fn context(budget_tokens: u32) -> RankingContext {
        RankingContext {
            project_id: Some("project-1".into()),
            workspace_key: Some("workspace-1".into()),
            current_conversation_id: "current".into(),
            reset_at: Some(100),
            now: 10 * 86_400_000,
            budget_tokens,
        }
    }

    #[test]
    fn conservative_estimator_and_capacity_budget_are_bounded() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(retrieval_budget(Some(8_192)), 819);
        assert_eq!(retrieval_budget(Some(100_000)), 2_000);
        assert_eq!(retrieval_budget(None), 2_000);
    }

    #[test]
    fn exact_scope_precedes_relevant_global_and_irrelevant_global_is_omitted() {
        let mut exact = entry("exact", "unrelated scoped observation");
        exact.project_id = Some("project-1".into());
        exact.workspace_key = Some("workspace-1".into());
        let relevant = entry("relevant", "rust memory ranking");
        let irrelevant = entry("irrelevant", "weather report");

        let selected = select_entries("rust ranking", vec![irrelevant, relevant, exact], &context(2_000));
        assert_eq!(
            selected
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["exact", "relevant"],
        );
    }

    #[test]
    fn protected_kind_recency_and_diversity_scoring_is_deterministic() {
        let mut pinned = entry("pinned", "needle");
        pinned.pinned = true;
        pinned.updated_at = 1;
        let mut edited = entry("edited", "needle");
        edited.user_edited = true;
        edited.updated_at = 1;
        let mut decision = entry("decision", "needle");
        decision.kind = "decision".into();
        decision.updated_at = 1;
        let mut diverse = entry("diverse", "needle");
        diverse.sources.push(source("diverse", "source-2"));
        let mut recent = entry("recent", "needle");
        recent.updated_at = context(2_000).now;
        let old = entry("old", "needle");

        let selection = select_entries(
            "needle",
            vec![old, recent, diverse, decision, edited, pinned],
            &context(2_000),
        );
        assert_eq!(
            selection
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["pinned", "edited", "decision", "recent", "diverse", "old"],
        );
        let repeat = select_entries("needle", selection.entries.clone(), &context(2_000));
        assert_eq!(selection.entries, repeat.entries);
    }

    #[test]
    fn lifecycle_reset_and_current_conversation_only_entries_are_filtered() {
        let mut deleted = entry("deleted", "needle");
        deleted.state = "deleted".into();
        let mut conflict = entry("conflict", "needle");
        conflict.state = "conflict".into();
        let mut superseded = entry("superseded", "needle");
        superseded.state = "superseded".into();
        let mut pre_reset = entry("pre-reset", "needle");
        pre_reset.updated_at = 99;
        pre_reset.sources[0].last_observed_at = 99;
        let mut current_only = entry("current-only", "needle");
        current_only.sources = vec![source("current-only", "current")];
        let mut mixed = entry("mixed", "needle");
        mixed.sources.push(source("mixed", "current"));
        let mut old_foreign_new_current = entry("old-foreign-new-current", "needle");
        old_foreign_new_current.sources[0].last_observed_at = 99;
        old_foreign_new_current
            .sources
            .push(source("old-foreign-new-current", "current"));

        let selected = select_entries(
            "needle",
            vec![
                deleted,
                conflict,
                superseded,
                pre_reset,
                current_only,
                mixed,
                old_foreign_new_current,
            ],
            &context(2_000),
        );
        assert_eq!(
            selected
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["mixed"]
        );
    }

    #[test]
    fn selection_never_partially_truncates_an_entry() {
        let first = entry("first", "needle compact");
        let second = entry("second", &format!("needle {}", "x".repeat(300)));
        let budget = estimate_tokens(first.content.as_deref().unwrap());
        let selected = select_entries("needle", vec![second, first], &context(budget));
        assert_eq!(selected.entries.len(), 1);
        assert_eq!(selected.entries[0].id, "first");
        assert!(selected.estimated_tokens <= budget);
    }
}
