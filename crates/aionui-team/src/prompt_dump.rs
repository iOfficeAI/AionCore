use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::types::TeammateRole;

#[derive(Clone, Debug, Default)]
pub struct TeamPromptDumpConfig {
    dump_dir: Option<PathBuf>,
}

impl TeamPromptDumpConfig {
    pub fn disabled() -> Self {
        Self { dump_dir: None }
    }

    pub fn enabled(dump_dir: impl AsRef<Path>) -> Self {
        Self {
            dump_dir: Some(dump_dir.as_ref().to_path_buf()),
        }
    }

    pub fn from_data_dir(data_dir: impl AsRef<Path>, enabled: bool) -> Self {
        if enabled {
            Self::enabled(data_dir.as_ref().join("prompt-dumps"))
        } else {
            Self::disabled()
        }
    }

    fn dump_dir(&self) -> Option<&Path> {
        self.dump_dir.as_deref()
    }
}

pub(crate) struct TeamWakePromptDump<'a> {
    pub team_id: &'a str,
    pub slot_id: &'a str,
    pub conversation_id: &'a str,
    pub role: TeammateRole,
    pub needs_role_prompt: bool,
    pub unread_count: usize,
    pub prompt: &'a str,
}

pub(crate) struct TeamToolsListDump<'a> {
    pub team_id: &'a str,
    pub caller_slot_id: &'a str,
    pub caller_role: TeammateRole,
    pub tools: &'a [Value],
}

pub(crate) fn dump_team_wake_prompt(
    config: &TeamPromptDumpConfig,
    dump: TeamWakePromptDump<'_>,
) -> io::Result<Option<PathBuf>> {
    let Some(dump_dir) = config.dump_dir() else {
        return Ok(None);
    };
    fs::create_dir_all(dump_dir)?;

    let created_at_ms = current_time_ms();
    let path = dump_dir.join(format!(
        "{}-team-wake-prompt-{}-{}.txt",
        current_time_nanos(),
        sanitize_segment(dump.team_id),
        sanitize_segment(dump.slot_id)
    ));

    let body = format!(
        "kind: team-wake-prompt\nscope: team-wake-content-only\nnot_final_agent_input: true\nteam_id: {}\nslot_id: {}\nconversation_id: {}\nrole: {}\nneeds_role_prompt: {}\nunread_count: {}\ncreated_at_ms: {}\n\n---- prompt ----\n{}\n",
        dump.team_id,
        dump.slot_id,
        dump.conversation_id,
        role_label(dump.role),
        dump.needs_role_prompt,
        dump.unread_count,
        created_at_ms,
        dump.prompt
    );
    fs::write(&path, body)?;
    Ok(Some(path))
}

pub(crate) fn dump_team_tools_list(
    config: &TeamPromptDumpConfig,
    dump: TeamToolsListDump<'_>,
) -> io::Result<Option<PathBuf>> {
    let Some(dump_dir) = config.dump_dir() else {
        return Ok(None);
    };
    fs::create_dir_all(dump_dir)?;

    let path = dump_dir.join(format!(
        "{}-team-tools-list-{}-{}.json",
        current_time_nanos(),
        sanitize_segment(dump.team_id),
        sanitize_segment(dump.caller_slot_id)
    ));
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "kind": "team-tools-list",
        "scope": "team-mcp-server-tools-only",
        "not_final_agent_tools": true,
        "team_id": dump.team_id,
        "caller_slot_id": dump.caller_slot_id,
        "caller_role": role_label(dump.caller_role),
        "created_at_ms": current_time_ms(),
        "tools": dump.tools,
    }))?;
    fs::write(&path, body)?;
    Ok(Some(path))
}

fn role_label(role: TeammateRole) -> &'static str {
    match role {
        TeammateRole::Lead => "lead",
        TeammateRole::Teammate => "teammate",
    }
}

fn current_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn current_time_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn sanitize_segment(value: &str) -> String {
    let segment: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    if segment.is_empty() { "none".to_owned() } else { segment }
}
