use std::collections::HashMap;
use std::fmt::Write;

use crate::error::TeamError;
use crate::service::TeamSessionService;
use crate::service::spawn_support::resolve_runtime_backend;
use aionui_db::models::AssistantDefinitionRow;

impl TeamSessionService {
    pub(crate) async fn describe_assistant(
        &self,
        assistant_id: &str,
        locale: Option<&str>,
    ) -> Result<String, TeamError> {
        let definition = self
            .assistant_definition_repo
            .get_by_assistant_id(assistant_id)
            .await?
            .ok_or_else(|| TeamError::InvalidRequest(format!("Preset assistant not found: {assistant_id}")))?;
        let overlay = self.assistant_overlay_repo.get(&definition.id).await?;
        let effective_agent_id = overlay
            .as_ref()
            .and_then(|row| row.agent_id_override.as_deref())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(definition.agent_id.as_str());
        let effective_backend = resolve_runtime_backend(&self.agent_metadata_repo, effective_agent_id).await?;

        Ok(render_assistant_description_json(
            &definition,
            &effective_backend,
            locale.unwrap_or("en-US"),
        )?)
    }
}

fn render_assistant_description_json(
    definition: &AssistantDefinitionRow,
    effective_backend: &str,
    locale: &str,
) -> Result<String, serde_json::Error> {
    let name_map = decode_str_map(&definition.name_i18n);
    let description_map = decode_str_map(&definition.description_i18n);
    let prompts_map = decode_list_map(&definition.recommended_prompts_i18n);

    let name = localized_text(&name_map, &definition.name, locale);
    let description = localized_optional_text(&description_map, definition.description.as_deref(), locale)
        .unwrap_or_else(|| "No description available.".to_owned());
    let example_tasks = localized_list(&prompts_map, &definition.recommended_prompts, locale).unwrap_or_default();
    let skills = decode_string_list(&definition.default_skill_ids)
        .into_iter()
        .chain(decode_string_list(&definition.custom_skill_names))
        .collect::<Vec<_>>();
    let description_markdown = render_assistant_description(definition, effective_backend, locale);

    serde_json::to_string_pretty(&serde_json::json!({
        "status": "ok",
        "assistant_id": definition.assistant_id,
        "name": name,
        "description": description,
        "description_markdown": description_markdown,
        "skills": skills,
        "example_tasks": example_tasks,
        "default_model": definition.default_model_value.clone(),
    }))
}

fn render_assistant_description(definition: &AssistantDefinitionRow, effective_backend: &str, locale: &str) -> String {
    let name_map = decode_str_map(&definition.name_i18n);
    let description_map = decode_str_map(&definition.description_i18n);
    let prompts_map = decode_list_map(&definition.recommended_prompts_i18n);

    let name = localized_text(&name_map, &definition.name, locale);
    let description = localized_optional_text(&description_map, definition.description.as_deref(), locale)
        .unwrap_or_else(|| "No description available.".to_owned());
    let example_tasks = localized_list(&prompts_map, &definition.recommended_prompts, locale).unwrap_or_default();
    let skills = decode_string_list(&definition.default_skill_ids)
        .into_iter()
        .chain(decode_string_list(&definition.custom_skill_names))
        .collect::<Vec<_>>();

    let mut out = String::new();
    let _ = writeln!(out, "# {} (`{}`)", name, definition.assistant_id);
    let _ = writeln!(out);
    let _ = writeln!(out, "Backend: {effective_backend}");
    let _ = writeln!(out);
    let _ = writeln!(out, "## Description");
    let _ = writeln!(out, "{description}");
    let _ = writeln!(out);
    let _ = writeln!(out, "## Skills");
    if skills.is_empty() {
        let _ = writeln!(out, "- None");
    } else {
        for skill in skills {
            let _ = writeln!(out, "- {skill}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Example tasks");
    if example_tasks.is_empty() {
        let _ = writeln!(out, "- None");
    } else {
        for task in example_tasks {
            let _ = writeln!(out, "- {task}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Use `team_spawn_agent` with `assistant_id=\"{}\"`.",
        definition.assistant_id
    );
    out.trim_end().to_owned()
}

fn decode_str_map(raw: &str) -> HashMap<String, String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_list_map(raw: &str) -> HashMap<String, Vec<String>> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_string_list(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn localized_text(map: &HashMap<String, String>, fallback: &str, locale: &str) -> String {
    map.get(locale)
        .or_else(|| map.get("en-US"))
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

fn localized_optional_text(map: &HashMap<String, String>, fallback: Option<&str>, locale: &str) -> Option<String> {
    map.get(locale)
        .or_else(|| map.get("en-US"))
        .cloned()
        .or_else(|| fallback.map(str::to_owned))
}

fn localized_list(map: &HashMap<String, Vec<String>>, fallback_raw: &str, locale: &str) -> Option<Vec<String>> {
    map.get(locale).or_else(|| map.get("en-US")).cloned().or_else(|| {
        let fallback = decode_string_list(fallback_raw);
        if fallback.is_empty() { None } else { Some(fallback) }
    })
}
