use std::cmp::Ordering;
use std::collections::HashSet;

use crate::types::{TaskStatus, TeamAgent, TeamTask, TeammateRole};

pub(super) const MAX_WAKE_TASK_ROWS: usize = 40;
pub(super) const MAX_RECENT_COMPLETED_ROWS: usize = 8;
pub(super) const MAX_TEAMMATE_RECENT_COMPLETED_ROWS: usize = 3;
pub(super) const MAX_SUBJECT_CHARS: usize = 120;
pub(super) const MAX_BLOCKED_BY_IDS: usize = 5;

pub(super) fn render_task_board_summary(
    agent: &TeamAgent,
    tasks: &[TeamTask],
    current_slot_ids: &HashSet<String>,
) -> String {
    let mut output = String::with_capacity(1024);
    output.push_str("## Current Task Board Summary\n\n");

    if tasks.is_empty() {
        output.push_str("No tasks on the board.\n\n");
        return output;
    }

    let visible_tasks = filter_summary_input(tasks, current_slot_ids);
    let selection = select_summary_tasks(agent, &visible_tasks);
    let selected = selection.tasks;
    let displayed_ids: HashSet<&str> = selected.iter().map(|task| task.id.as_str()).collect();
    let hidden_active = tasks
        .iter()
        .filter(|task| is_active(task.status))
        .filter(|task| !displayed_ids.contains(task.id.as_str()))
        .count();
    let displayed_completed = selected
        .iter()
        .filter(|task| task.status == TaskStatus::Completed)
        .count();
    let total_completed = tasks.iter().filter(|task| task.status == TaskStatus::Completed).count();
    let total_deleted = tasks.iter().filter(|task| task.status == TaskStatus::Deleted).count();
    let hidden_completed = total_completed.saturating_sub(displayed_completed);
    let hidden_deleted = total_deleted;

    output.push_str(&format!("Showing {} of {} tasks.\n", selected.len(), tasks.len()));
    output.push_str(&format!(
        "Hidden: active={hidden_active}, completed={hidden_completed}, deleted={hidden_deleted}.\n"
    ));
    if hidden_active + hidden_completed + hidden_deleted > 0 {
        output.push_str("More tasks are available via `team_task_list`; use filters when needed.\n");
    }
    output.push('\n');
    output.push_str("| ID | Subject | Status | Owner | Blocked By |\n");
    output.push_str("|---|---|---|---|---|\n");
    for task in selected {
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            short_task_id(&task.id),
            truncate_subject(&task.subject),
            task.status,
            render_owner(task, current_slot_ids),
            render_blocked_by(&task.blocked_by),
        ));
    }
    output.push('\n');
    output
}

struct SummarySelection<'a> {
    tasks: Vec<&'a TeamTask>,
}

fn filter_summary_input<'a>(tasks: &'a [TeamTask], current_slot_ids: &HashSet<String>) -> Vec<&'a TeamTask> {
    let visible_base_ids: HashSet<&str> = tasks
        .iter()
        .filter(|task| is_active(task.status))
        .filter(|task| is_current_or_ownerless(task, current_slot_ids))
        .map(|task| task.id.as_str())
        .collect();

    let visible_blocker_ids: HashSet<&str> = tasks
        .iter()
        .filter(|task| visible_base_ids.contains(task.id.as_str()))
        .flat_map(|task| task.blocked_by.iter().map(String::as_str))
        .collect();

    tasks
        .iter()
        .filter(|task| {
            if is_current_or_ownerless(task, current_slot_ids) {
                return true;
            }

            is_active(task.status)
                && is_removed_owner(task, current_slot_ids)
                && visible_blocker_ids.contains(task.id.as_str())
        })
        .collect()
}

fn select_summary_tasks<'a>(agent: &TeamAgent, tasks: &[&'a TeamTask]) -> SummarySelection<'a> {
    let own_active_ids: HashSet<&str> = tasks
        .iter()
        .copied()
        .filter(|task| is_active(task.status))
        .filter(|task| task.owner.as_deref() == Some(agent.slot_id.as_str()))
        .map(|task| task.id.as_str())
        .collect();

    let blocker_ids: HashSet<&str> = tasks
        .iter()
        .copied()
        .filter(|task| own_active_ids.contains(task.id.as_str()))
        .flat_map(|task| task.blocked_by.iter().map(String::as_str))
        .collect();

    let mut blocked_by_own_ids: HashSet<&str> = tasks
        .iter()
        .copied()
        .filter(|task| own_active_ids.contains(task.id.as_str()))
        .flat_map(|task| task.blocks.iter().map(String::as_str))
        .collect();
    blocked_by_own_ids.extend(
        tasks
            .iter()
            .copied()
            .filter(|task| is_active(task.status))
            .filter(|task| task.blocked_by.iter().any(|id| own_active_ids.contains(id.as_str())))
            .map(|task| task.id.as_str()),
    );

    if agent.role == TeammateRole::Teammate {
        return select_teammate_summary_tasks(tasks, &own_active_ids, &blocker_ids, &blocked_by_own_ids, agent);
    }

    select_leader_summary_tasks(tasks, &own_active_ids, &blocker_ids, &blocked_by_own_ids)
}

fn select_leader_summary_tasks<'a>(
    tasks: &[&'a TeamTask],
    own_active_ids: &HashSet<&str>,
    blocker_ids: &HashSet<&str>,
    blocked_by_own_ids: &HashSet<&str>,
) -> SummarySelection<'a> {
    let mut active: Vec<&TeamTask> = tasks.iter().copied().filter(|task| is_active(task.status)).collect();
    active.sort_by(|left, right| compare_active_tasks(left, right, own_active_ids, blocker_ids, blocked_by_own_ids));

    let mut selected: Vec<&TeamTask> = active.into_iter().take(MAX_WAKE_TASK_ROWS).collect();
    if selected.len() < MAX_WAKE_TASK_ROWS {
        let mut completed: Vec<&TeamTask> = tasks
            .iter()
            .copied()
            .filter(|task| task.status == TaskStatus::Completed)
            .collect();
        completed.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        let remaining = MAX_WAKE_TASK_ROWS - selected.len();
        selected.extend(completed.into_iter().take(MAX_RECENT_COMPLETED_ROWS.min(remaining)));
    }

    SummarySelection { tasks: selected }
}

fn select_teammate_summary_tasks<'a>(
    tasks: &[&'a TeamTask],
    own_active_ids: &HashSet<&str>,
    blocker_ids: &HashSet<&str>,
    blocked_by_own_ids: &HashSet<&str>,
    agent: &TeamAgent,
) -> SummarySelection<'a> {
    let mut relevant_active: Vec<&TeamTask> = tasks
        .iter()
        .copied()
        .filter(|task| is_active(task.status))
        .filter(|task| {
            own_active_ids.contains(task.id.as_str())
                || blocker_ids.contains(task.id.as_str())
                || blocked_by_own_ids.contains(task.id.as_str())
        })
        .collect();
    relevant_active
        .sort_by(|left, right| compare_active_tasks(left, right, own_active_ids, blocker_ids, blocked_by_own_ids));

    let mut selected: Vec<&TeamTask> = relevant_active.into_iter().take(MAX_WAKE_TASK_ROWS).collect();
    if selected.len() < MAX_WAKE_TASK_ROWS {
        let mut completed: Vec<&TeamTask> = tasks
            .iter()
            .copied()
            .filter(|task| task.status == TaskStatus::Completed)
            .filter(|task| task.owner.as_deref() == Some(agent.slot_id.as_str()))
            .collect();
        completed.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        let remaining = MAX_WAKE_TASK_ROWS - selected.len();
        selected.extend(
            completed
                .into_iter()
                .take(MAX_TEAMMATE_RECENT_COMPLETED_ROWS.min(remaining)),
        );
    }

    SummarySelection { tasks: selected }
}

fn compare_active_tasks(
    left: &TeamTask,
    right: &TeamTask,
    own_active_ids: &HashSet<&str>,
    blocker_ids: &HashSet<&str>,
    blocked_by_own_ids: &HashSet<&str>,
) -> Ordering {
    active_rank(left, own_active_ids, blocker_ids, blocked_by_own_ids)
        .cmp(&active_rank(right, own_active_ids, blocker_ids, blocked_by_own_ids))
        .then_with(|| status_rank(left.status).cmp(&status_rank(right.status)))
        .then_with(|| compare_same_status_recency(left, right))
        .then_with(|| left.id.cmp(&right.id))
}

fn compare_same_status_recency(left: &TeamTask, right: &TeamTask) -> Ordering {
    match left.status {
        TaskStatus::InProgress => right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.created_at.cmp(&right.created_at)),
        TaskStatus::Pending => right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.created_at.cmp(&right.created_at)),
        TaskStatus::Completed | TaskStatus::Deleted => Ordering::Equal,
    }
}

fn active_rank(
    task: &TeamTask,
    own_active_ids: &HashSet<&str>,
    blocker_ids: &HashSet<&str>,
    blocked_by_own_ids: &HashSet<&str>,
) -> u8 {
    if own_active_ids.contains(task.id.as_str()) {
        0
    } else if blocker_ids.contains(task.id.as_str()) {
        1
    } else if blocked_by_own_ids.contains(task.id.as_str()) {
        2
    } else if task.status == TaskStatus::InProgress {
        3
    } else {
        4
    }
}

fn status_rank(status: TaskStatus) -> u8 {
    match status {
        TaskStatus::InProgress => 0,
        TaskStatus::Pending => 1,
        TaskStatus::Completed => 2,
        TaskStatus::Deleted => 3,
    }
}

fn is_active(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Pending | TaskStatus::InProgress)
}

fn is_current_or_ownerless(task: &TeamTask, current_slot_ids: &HashSet<String>) -> bool {
    match task.owner.as_deref() {
        Some(owner) => current_slot_ids.contains(owner),
        None => true,
    }
}

fn is_removed_owner(task: &TeamTask, current_slot_ids: &HashSet<String>) -> bool {
    task.owner
        .as_deref()
        .is_some_and(|owner| !current_slot_ids.contains(owner))
}

fn render_owner(task: &TeamTask, current_slot_ids: &HashSet<String>) -> String {
    match task.owner.as_deref() {
        Some(owner) if current_slot_ids.contains(owner) => owner.to_owned(),
        Some(owner) => format!("{owner} (removed)"),
        None => "-".to_owned(),
    }
}

fn short_task_id(id: &str) -> String {
    format!("{}…", id.chars().take(8).collect::<String>())
}

fn truncate_subject(subject: &str) -> String {
    if subject.chars().count() <= MAX_SUBJECT_CHARS {
        subject.to_owned()
    } else {
        format!("{}...", subject.chars().take(MAX_SUBJECT_CHARS).collect::<String>())
    }
}

fn render_blocked_by(blocked_by: &[String]) -> String {
    if blocked_by.is_empty() {
        return "-".to_owned();
    }
    let mut parts: Vec<String> = blocked_by
        .iter()
        .take(MAX_BLOCKED_BY_IDS)
        .map(|id| short_task_id(id))
        .collect();
    if blocked_by.len() > MAX_BLOCKED_BY_IDS {
        parts.push(format!("+{} more", blocked_by.len() - MAX_BLOCKED_BY_IDS));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn agent(slot_id: &str) -> TeamAgent {
        agent_with_role(slot_id, TeammateRole::Lead)
    }

    fn teammate(slot_id: &str) -> TeamAgent {
        agent_with_role(slot_id, TeammateRole::Teammate)
    }

    fn agent_with_role(slot_id: &str, role: TeammateRole) -> TeamAgent {
        TeamAgent {
            slot_id: slot_id.to_owned(),
            name: "Worker".to_owned(),
            role,
            conversation_id: "conv-1".to_owned(),
            backend: "acp".to_owned(),
            model: "claude".to_owned(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    fn task(
        id: &str,
        subject: &str,
        status: TaskStatus,
        owner: Option<&str>,
        created_at: i64,
        updated_at: i64,
    ) -> TeamTask {
        TeamTask {
            id: id.to_owned(),
            team_id: "team-1".to_owned(),
            subject: subject.to_owned(),
            description: None,
            status,
            owner: owner.map(str::to_owned),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            metadata: None,
            created_at,
            updated_at,
        }
    }

    fn row_count(payload: &str) -> usize {
        payload
            .lines()
            .filter(|line| line.starts_with("| ") && !line.starts_with("| ID ") && !line.starts_with("|---"))
            .count()
    }

    fn roster(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn default_roster() -> HashSet<String> {
        roster(&[
            "worker-1",
            "worker-2",
            "worker-3",
            "worker-4",
            "019f4056-fd18-7411-ab09-8868ff17cb36",
        ])
    }

    #[test]
    fn empty_board_renders_empty_summary() {
        let payload = render_task_board_summary(&agent("worker-1"), &[], &roster(&["worker-1"]));
        assert!(payload.contains("## Current Task Board Summary"));
        assert!(payload.contains("No tasks on the board."));
    }

    #[test]
    fn active_tasks_under_limit_all_render() {
        let tasks = vec![
            task(
                "aaaaaaaa-1111",
                "Owned active",
                TaskStatus::InProgress,
                Some("worker-1"),
                1,
                10,
            ),
            task(
                "bbbbbbbb-1111",
                "Other pending",
                TaskStatus::Pending,
                Some("worker-2"),
                2,
                20,
            ),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.contains("Showing 2 of 2 tasks."));
        assert!(payload.contains("Hidden: active=0, completed=0, deleted=0."));
        assert!(payload.contains("| aaaaaaaa… | Owned active | in_progress | worker-1 | - |"));
        assert!(payload.contains("| bbbbbbbb… | Other pending | pending | worker-2 | - |"));
    }

    #[test]
    fn completed_tasks_are_limited_to_recent_rows() {
        let tasks: Vec<_> = (0..10)
            .map(|index| {
                task(
                    &format!("done{index:04}-1111"),
                    &format!("Completed {index}"),
                    TaskStatus::Completed,
                    Some("worker-2"),
                    index,
                    index,
                )
            })
            .collect();
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert_eq!(row_count(&payload), MAX_RECENT_COMPLETED_ROWS);
        assert!(payload.contains("Completed 9"));
        assert!(payload.contains("Completed 2"));
        assert!(!payload.contains("Completed 1"));
        assert!(payload.contains("Hidden: active=0, completed=2, deleted=0."));
        assert!(payload.contains("More tasks are available via `team_task_list`; use filters when needed."));
        assert!(!payload.contains("team_task_list({})"));
        assert!(!payload.contains(r#"team_task_list({"status":["pending","in_progress"]})"#));
    }

    #[test]
    fn deleted_tasks_are_hidden_and_counted() {
        let tasks = vec![
            task("aaaaaaaa-1111", "Active", TaskStatus::Pending, Some("worker-1"), 1, 1),
            task(
                "dddddddd-1111",
                "Deleted history",
                TaskStatus::Deleted,
                Some("worker-1"),
                2,
                2,
            ),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.contains("Active"));
        assert!(!payload.contains("Deleted history"));
        assert!(payload.contains("Hidden: active=0, completed=0, deleted=1."));
    }

    #[test]
    fn active_tasks_over_limit_are_truncated_and_counted() {
        let tasks: Vec<_> = (0..45)
            .map(|index| {
                task(
                    &format!("task{index:04}-1111"),
                    &format!("Active {index}"),
                    TaskStatus::Pending,
                    Some("worker-2"),
                    index,
                    index,
                )
            })
            .collect();
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert_eq!(row_count(&payload), MAX_WAKE_TASK_ROWS);
        assert!(payload.contains("Showing 40 of 45 tasks."));
        assert!(payload.contains("Hidden: active=5, completed=0, deleted=0."));
        assert!(payload.contains("| task0044… | Active 44 | pending | worker-2 | - |"));
        assert!(!payload.contains("| task0004… | Active 4 | pending | worker-2 | - |"));
    }

    #[test]
    fn owned_active_tasks_sort_before_other_active_tasks() {
        let tasks = vec![
            task(
                "other111-1111",
                "Other new",
                TaskStatus::InProgress,
                Some("worker-2"),
                1,
                100,
            ),
            task(
                "owned111-1111",
                "Owned old",
                TaskStatus::Pending,
                Some("worker-1"),
                2,
                1,
            ),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.find("Owned old").unwrap() < payload.find("Other new").unwrap());
    }

    #[test]
    fn active_dependency_neighbors_sort_after_owned_tasks() {
        let mut owned = task("ownedaaa-1111", "Owned", TaskStatus::InProgress, Some("worker-1"), 1, 1);
        owned.blocked_by = vec!["blockera-1111".to_owned()];
        owned.blocks = vec!["downstrm-1111".to_owned()];
        let blocker = task("blockera-1111", "Blocker", TaskStatus::Pending, Some("worker-2"), 2, 20);
        let downstream = task(
            "downstrm-1111",
            "Downstream",
            TaskStatus::Pending,
            Some("worker-3"),
            3,
            30,
        );
        let other = task(
            "otheraaa-1111",
            "Other",
            TaskStatus::InProgress,
            Some("worker-4"),
            4,
            40,
        );
        let payload = render_task_board_summary(
            &agent("worker-1"),
            &[other, downstream, blocker, owned],
            &default_roster(),
        );
        assert!(payload.find("Owned").unwrap() < payload.find("Blocker").unwrap());
        assert!(payload.find("Blocker").unwrap() < payload.find("Downstream").unwrap());
        assert!(payload.find("Downstream").unwrap() < payload.find("Other").unwrap());
    }

    #[test]
    fn teammate_summary_keeps_owned_and_related_active_tasks_only() {
        let mut owned = task(
            "ownedaaa-1111",
            "Owned active",
            TaskStatus::InProgress,
            Some("worker-1"),
            1,
            10,
        );
        owned.blocked_by = vec!["blockera-1111".to_owned()];
        owned.blocks = vec!["downstrm-1111".to_owned()];
        let blocker = task(
            "blockera-1111",
            "Active blocker",
            TaskStatus::Pending,
            Some("worker-2"),
            2,
            20,
        );
        let downstream = task(
            "downstrm-1111",
            "Active downstream",
            TaskStatus::Pending,
            Some("worker-3"),
            3,
            30,
        );
        let unrelated_active = task(
            "unrelact-1111",
            "Unrelated active",
            TaskStatus::InProgress,
            Some("worker-4"),
            4,
            40,
        );
        let own_completed = task(
            "owndonea-1111",
            "Own completed",
            TaskStatus::Completed,
            Some("worker-1"),
            5,
            50,
        );
        let unrelated_completed = task(
            "othdonea-1111",
            "Other completed",
            TaskStatus::Completed,
            Some("worker-4"),
            6,
            60,
        );

        let payload = render_task_board_summary(
            &teammate("worker-1"),
            &[
                unrelated_active,
                unrelated_completed,
                own_completed,
                downstream,
                blocker,
                owned,
            ],
            &default_roster(),
        );

        assert!(payload.contains("Owned active"));
        assert!(payload.contains("Active blocker"));
        assert!(payload.contains("Active downstream"));
        assert!(payload.contains("Own completed"));
        assert!(!payload.contains("Unrelated active"));
        assert!(!payload.contains("Other completed"));
        assert!(payload.contains("Hidden: active=1, completed=1, deleted=0."));
    }

    #[test]
    fn teammate_recent_completed_rows_are_limited_to_three_owned_tasks() {
        let mut tasks = vec![task(
            "otherdon-1111",
            "Other newest completed",
            TaskStatus::Completed,
            Some("worker-2"),
            10,
            100,
        )];
        tasks.extend((0..4).map(|index| {
            task(
                &format!("owndone{index}-1111"),
                &format!("Own completed {index}"),
                TaskStatus::Completed,
                Some("worker-1"),
                index,
                index,
            )
        }));

        let payload = render_task_board_summary(&teammate("worker-1"), &tasks, &default_roster());

        assert_eq!(row_count(&payload), 3);
        assert!(payload.contains("Own completed 3"));
        assert!(payload.contains("Own completed 1"));
        assert!(!payload.contains("Own completed 0"));
        assert!(!payload.contains("Other newest completed"));
    }

    #[test]
    fn reverse_blocked_by_downstream_sorts_after_owned_tasks() {
        let owned = task("ownedaaa-1111", "Owned", TaskStatus::InProgress, Some("worker-1"), 1, 1);
        let mut downstream = task(
            "downstrm-1111",
            "Downstream reverse",
            TaskStatus::Pending,
            Some("worker-3"),
            3,
            30,
        );
        downstream.blocked_by = vec!["ownedaaa-1111".to_owned()];
        let other = task(
            "otheraaa-1111",
            "Other",
            TaskStatus::InProgress,
            Some("worker-4"),
            4,
            40,
        );
        let payload = render_task_board_summary(&agent("worker-1"), &[other, downstream, owned], &default_roster());
        assert!(payload.find("Owned").unwrap() < payload.find("Downstream reverse").unwrap());
        assert!(payload.find("Downstream reverse").unwrap() < payload.find("Other").unwrap());
    }

    #[test]
    fn completed_and_deleted_do_not_create_dependency_priority() {
        let mut owned = task("ownedaaa-1111", "Owned", TaskStatus::InProgress, Some("worker-1"), 1, 1);
        owned.blocked_by = vec!["doneaaaa-1111".to_owned(), "delddddd-1111".to_owned()];
        let completed = task(
            "doneaaaa-1111",
            "Completed blocker",
            TaskStatus::Completed,
            Some("worker-2"),
            2,
            200,
        );
        let deleted = task(
            "delddddd-1111",
            "Deleted blocker",
            TaskStatus::Deleted,
            Some("worker-2"),
            3,
            300,
        );
        let other = task(
            "otheraaa-1111",
            "Other active",
            TaskStatus::InProgress,
            Some("worker-3"),
            4,
            400,
        );
        let payload = render_task_board_summary(
            &agent("worker-1"),
            &[completed, deleted, other, owned],
            &default_roster(),
        );
        assert!(payload.find("Owned").unwrap() < payload.find("Other active").unwrap());
        assert!(payload.find("Other active").unwrap() < payload.find("Completed blocker").unwrap());
        assert!(!payload.contains("Deleted blocker"));
    }

    #[test]
    fn same_priority_sort_is_stable_and_deterministic() {
        let tasks = vec![
            task(
                "cccccccc-1111",
                "Pending newest",
                TaskStatus::Pending,
                Some("worker-2"),
                1,
                300,
            ),
            task(
                "bbbbbbbb-1111",
                "In progress old",
                TaskStatus::InProgress,
                Some("worker-2"),
                2,
                10,
            ),
            task(
                "aaaaaaaa-1111",
                "In progress new",
                TaskStatus::InProgress,
                Some("worker-2"),
                3,
                20,
            ),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.find("In progress new").unwrap() < payload.find("In progress old").unwrap());
        assert!(payload.find("In progress old").unwrap() < payload.find("Pending newest").unwrap());
    }

    #[test]
    fn pending_tasks_with_same_priority_sort_by_updated_at_desc() {
        let tasks = vec![
            task(
                "aaaaaaaa-1111",
                "Pending older update",
                TaskStatus::Pending,
                Some("worker-2"),
                1,
                10,
            ),
            task(
                "bbbbbbbb-1111",
                "Pending newer update",
                TaskStatus::Pending,
                Some("worker-2"),
                2,
                20,
            ),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.find("Pending newer update").unwrap() < payload.find("Pending older update").unwrap());
    }

    #[test]
    fn hidden_counts_are_mutually_exclusive() {
        let mut tasks: Vec<TeamTask> = (0..42)
            .map(|index| {
                task(
                    &format!("act{index:05}-1111"),
                    &format!("Active {index}"),
                    TaskStatus::Pending,
                    Some("worker-2"),
                    index,
                    index,
                )
            })
            .collect();
        tasks.push(task(
            "doneaaaa-1111",
            "Completed",
            TaskStatus::Completed,
            Some("worker-2"),
            100,
            100,
        ));
        tasks.push(task(
            "delddddd-1111",
            "Deleted",
            TaskStatus::Deleted,
            Some("worker-2"),
            101,
            101,
        ));
        let payload = render_task_board_summary(&agent("worker-1"), &tasks, &default_roster());
        assert!(payload.contains("Showing 40 of 44 tasks."));
        assert!(payload.contains("Hidden: active=2, completed=1, deleted=1."));
    }

    #[test]
    fn subject_and_blocked_by_values_are_truncated() {
        let mut active = task(
            "aaaaaaaa-1111",
            &"x".repeat(MAX_SUBJECT_CHARS + 5),
            TaskStatus::Pending,
            Some("worker-1"),
            1,
            1,
        );
        active.blocked_by = vec![
            "block001-1111".to_owned(),
            "block002-1111".to_owned(),
            "block003-1111".to_owned(),
            "block004-1111".to_owned(),
            "block005-1111".to_owned(),
            "block006-1111".to_owned(),
        ];
        let payload = render_task_board_summary(&agent("worker-1"), &[active], &default_roster());
        assert!(payload.contains(&format!("{}...", "x".repeat(MAX_SUBJECT_CHARS))));
        assert!(payload.contains("block001…"));
        assert!(payload.contains("+1 more"));
        assert!(!payload.contains("block006"));
    }

    #[test]
    fn task_ids_are_short_but_owner_slot_id_is_full() {
        let mut active = task(
            "aaaaaaaa-1234-5678",
            "Active",
            TaskStatus::Pending,
            Some("019f4056-fd18-7411-ab09-8868ff17cb36"),
            1,
            1,
        );
        active.blocked_by = vec!["bbbbbbbb-1234-5678".to_owned()];
        let payload = render_task_board_summary(&agent("worker-1"), &[active], &default_roster());
        assert!(
            payload.contains("| aaaaaaaa… | Active | pending | 019f4056-fd18-7411-ab09-8868ff17cb36 | bbbbbbbb… |")
        );
        assert!(!payload.contains("bbbbbbbb-1234-5678"));
    }

    #[test]
    fn removed_owner_active_task_is_hidden_when_unrelated() {
        let removed = task(
            "removeda-1111",
            "Removed member active",
            TaskStatus::InProgress,
            Some("worker-2"),
            1,
            10,
        );
        let current = task(
            "currenta-1111",
            "Current member active",
            TaskStatus::Pending,
            Some("worker-1"),
            2,
            20,
        );

        let payload =
            render_task_board_summary(&agent("lead-1"), &[removed, current], &roster(&["lead-1", "worker-1"]));

        assert!(payload.contains("Current member active"));
        assert!(!payload.contains("Removed member active"));
        assert!(!payload.contains("worker-2 (removed)"));
        assert!(payload.contains("Showing 1 of 2 tasks."));
        assert!(payload.contains("Hidden: active=1, completed=0, deleted=0."));
    }

    #[test]
    fn removed_owner_active_task_is_shown_when_it_blocks_visible_work() {
        let blocker = task(
            "blockera-1111",
            "Removed member blocker",
            TaskStatus::InProgress,
            Some("worker-2"),
            1,
            10,
        );
        let mut current = task(
            "currenta-1111",
            "Current member blocked",
            TaskStatus::Pending,
            Some("worker-1"),
            2,
            20,
        );
        current.blocked_by = vec!["blockera-1111".to_owned()];

        let payload =
            render_task_board_summary(&agent("lead-1"), &[blocker, current], &roster(&["lead-1", "worker-1"]));

        assert!(payload.contains("Removed member blocker"));
        assert!(payload.contains("Current member blocked"));
        assert!(payload.contains("| blockera… | Removed member blocker | in_progress | worker-2 (removed) | - |"));
        assert!(payload.contains("| currenta… | Current member blocked | pending | worker-1 | blockera… |"));
        assert!(payload.contains("Hidden: active=0, completed=0, deleted=0."));
    }

    #[test]
    fn removed_owner_active_task_is_shown_when_it_blocks_ownerless_visible_work() {
        let blocker = task(
            "blockera-1111",
            "Removed owner blocker",
            TaskStatus::Pending,
            Some("worker-2"),
            1,
            10,
        );
        let mut ownerless = task(
            "ownerlsa-1111",
            "Ownerless blocked task",
            TaskStatus::Pending,
            None,
            2,
            20,
        );
        ownerless.blocked_by = vec!["blockera-1111".to_owned()];

        let payload = render_task_board_summary(
            &agent("lead-1"),
            &[blocker, ownerless],
            &roster(&["lead-1", "worker-1"]),
        );

        assert!(payload.contains("Removed owner blocker"));
        assert!(payload.contains("Ownerless blocked task"));
        assert!(payload.contains("worker-2 (removed)"));
    }

    #[test]
    fn removed_owner_completed_and_deleted_tasks_are_hidden() {
        let completed = task(
            "doneaaaa-1111",
            "Removed completed",
            TaskStatus::Completed,
            Some("worker-2"),
            1,
            10,
        );
        let deleted = task(
            "delddddd-1111",
            "Removed deleted",
            TaskStatus::Deleted,
            Some("worker-2"),
            2,
            20,
        );
        let current = task(
            "currenta-1111",
            "Current active",
            TaskStatus::Pending,
            Some("worker-1"),
            3,
            30,
        );

        let payload = render_task_board_summary(
            &agent("lead-1"),
            &[completed, deleted, current],
            &roster(&["lead-1", "worker-1"]),
        );

        assert!(payload.contains("Current active"));
        assert!(!payload.contains("Removed completed"));
        assert!(!payload.contains("Removed deleted"));
        assert!(payload.contains("Hidden: active=0, completed=1, deleted=1."));
    }
}
