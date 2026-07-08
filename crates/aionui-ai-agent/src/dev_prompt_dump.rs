use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub(crate) struct PromptDump<'a> {
    pub kind: &'static str,
    pub backend: Option<&'a str>,
    pub conversation_id: &'a str,
    pub session_id: Option<&'a str>,
    pub msg_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub prompt: &'a str,
}

pub(crate) struct AgentFinalInputDump<'a> {
    pub kind: &'static str,
    pub backend: &'static str,
    pub conversation_id: &'a str,
    pub session_id: Option<&'a str>,
    pub msg_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub input: Value,
    pub resolved_context: Value,
}

pub(crate) fn dump_dir_for_data_dir(data_dir: &Path, enabled: bool) -> Option<PathBuf> {
    if enabled {
        Some(data_dir.join("prompt-dumps"))
    } else {
        None
    }
}

pub(crate) fn dump_prompt(dump_dir: &Path, dump: PromptDump<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(dump_dir)?;

    let created_at_ms = current_time_ms();
    let stamp = current_time_nanos();
    let discriminator = dump.msg_id.or(dump.session_id).or(dump.turn_id).unwrap_or("none");
    let file_name = format!(
        "{}-{}-{}-{}.txt",
        stamp,
        sanitize_segment(dump.kind),
        sanitize_segment(dump.conversation_id),
        sanitize_segment(discriminator)
    );
    let path = dump_dir.join(file_name);

    let body = format!(
        "kind: {}\nbackend: {}\nconversation_id: {}\nsession_id: {}\nmsg_id: {}\nturn_id: {}\ncreated_at_ms: {}\n\n---- prompt ----\n{}\n",
        dump.kind,
        dump.backend.unwrap_or("none"),
        dump.conversation_id,
        dump.session_id.unwrap_or("none"),
        dump.msg_id.unwrap_or("none"),
        dump.turn_id.unwrap_or("none"),
        created_at_ms,
        dump.prompt
    );
    fs::write(&path, body)?;
    Ok(path)
}

pub(crate) fn dump_agent_final_input(dump_dir: &Path, dump: AgentFinalInputDump<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(dump_dir)?;

    let created_at_ms = current_time_ms();
    let stamp = current_time_nanos();
    let discriminator = dump.msg_id.or(dump.session_id).or(dump.turn_id).unwrap_or("none");
    let file_name = format!(
        "{}-{}-{}-{}.json",
        stamp,
        sanitize_segment(dump.kind),
        sanitize_segment(dump.conversation_id),
        sanitize_segment(discriminator)
    );
    let path = dump_dir.join(file_name);

    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "kind": dump.kind,
        "backend": dump.backend,
        "conversation_id": dump.conversation_id,
        "session_id": dump.session_id.unwrap_or("none"),
        "msg_id": dump.msg_id.unwrap_or("none"),
        "turn_id": dump.turn_id.unwrap_or("none"),
        "created_at_ms": created_at_ms,
        "input": dump.input,
        "resolved_context": dump.resolved_context,
    }))?;
    fs::write(&path, body)?;
    Ok(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_prompt_writes_metadata_and_prompt_body() {
        let temp = tempfile::tempdir().unwrap();

        let path = dump_prompt(
            temp.path(),
            PromptDump {
                kind: "test-prompt",
                backend: Some("claude"),
                conversation_id: "conversation/123",
                session_id: Some("session-1"),
                msg_id: Some("msg-1"),
                turn_id: Some("turn-1"),
                prompt: "final prompt body",
            },
        )
        .unwrap();

        assert_eq!(path.parent(), Some(temp.path()));
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.contains("test-prompt"));
        assert!(file_name.contains("conversation_123"));
        assert!(file_name.contains("msg-1"));

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("kind: test-prompt\n"));
        assert!(content.contains("backend: claude\n"));
        assert!(content.contains("conversation_id: conversation/123\n"));
        assert!(content.contains("session_id: session-1\n"));
        assert!(content.contains("msg_id: msg-1\n"));
        assert!(content.contains("turn_id: turn-1\n"));
        assert!(content.ends_with("---- prompt ----\nfinal prompt body\n"));
    }

    #[test]
    fn dump_agent_final_input_writes_json_with_raw_values() {
        let temp = tempfile::tempdir().unwrap();

        let path = dump_agent_final_input(
            temp.path(),
            AgentFinalInputDump {
                kind: "acp-final-input",
                backend: "acp",
                conversation_id: "conversation/123",
                session_id: Some("session-1"),
                msg_id: Some("msg-1"),
                turn_id: Some("turn-1"),
                input: serde_json::json!({
                    "content": "[Assistant Rules]\nraw secret\n[/Assistant Rules]\n\nhello"
                }),
                resolved_context: serde_json::json!({
                    "mcp_servers": [{
                        "name": "raw-mcp",
                        "env": { "TOKEN": "raw-token-value" }
                    }]
                }),
            },
        )
        .unwrap();

        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.contains("acp-final-input"));
        assert!(file_name.contains("conversation_123"));
        assert!(file_name.contains("msg-1"));

        let dump: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(dump["kind"], "acp-final-input");
        assert_eq!(dump["backend"], "acp");
        assert_eq!(dump["conversation_id"], "conversation/123");
        assert_eq!(dump["session_id"], "session-1");
        assert_eq!(dump["msg_id"], "msg-1");
        assert_eq!(dump["turn_id"], "turn-1");
        assert_eq!(
            dump["input"]["content"],
            "[Assistant Rules]\nraw secret\n[/Assistant Rules]\n\nhello"
        );
        assert_eq!(
            dump["resolved_context"]["mcp_servers"][0]["env"]["TOKEN"],
            "raw-token-value"
        );
    }

    #[test]
    fn disabled_dump_prompts_returns_no_dump_dir() {
        let data_dir = Path::new("/Users/alice/.aionui-dev");

        assert!(dump_dir_for_data_dir(data_dir, false).is_none());
    }

    #[test]
    fn enabled_dump_prompts_uses_data_dir() {
        let data_dir = Path::new("/Users/alice/.aionui-dev");

        let dump_dir = dump_dir_for_data_dir(data_dir, true).unwrap();

        assert_eq!(dump_dir, Path::new("/Users/alice/.aionui-dev/prompt-dumps"));
    }
}
