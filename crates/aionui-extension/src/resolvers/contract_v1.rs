//! Plugin Contract v1 contribution resolvers (M9 E13).
//!
//! Resolves commands / panels / workPanelViews / agentTools / backgroundServices
//! into runtime-ready structs. Host wiring (palette invoke, panel iframe, tool
//! dispatch, service spawn) remains follow-up; this slice exposes them via the
//! registry and `/api/extensions/*` list routes.

use std::path::Path;

use tracing::warn;

use crate::asset_paths::{is_remote_asset_url, normalized_asset_url_path};
use crate::types::{
    ExtAgentTool, ExtBackgroundService, ExtCommand, ExtPanel, ExtWorkPanelView, ResolvedAgentTool,
    ResolvedBackgroundService, ResolvedCommand, ResolvedPanel, ResolvedWorkPanelView,
};

fn resolve_asset_url(extension_name: &str, raw: &str) -> Option<String> {
    if is_remote_asset_url(raw) {
        return Some(raw.to_owned());
    }
    let relative = normalized_asset_url_path(raw)?;
    Some(format!("/api/extensions/{extension_name}/assets/{relative}"))
}

pub fn resolve_commands(commands: &[ExtCommand], extension_name: &str) -> Vec<ResolvedCommand> {
    commands
        .iter()
        .filter_map(|cmd| {
            if cmd.id.is_empty() || cmd.title.is_empty() {
                warn!(
                    extension = extension_name,
                    command_id = %cmd.id,
                    "Skipping command with empty id or title"
                );
                return None;
            }
            Some(ResolvedCommand {
                extension_name: extension_name.to_owned(),
                id: format!("ext-{extension_name}-{}", cmd.id),
                title: cmd.title.clone(),
                keybinding: cmd.keybinding.clone(),
                when: cmd.when.clone(),
            })
        })
        .collect()
}

pub fn resolve_panels(panels: &[ExtPanel], extension_name: &str, _ext_dir: &Path) -> Vec<ResolvedPanel> {
    panels
        .iter()
        .filter_map(|panel| {
            if panel.id.is_empty() || panel.title.is_empty() {
                warn!(
                    extension = extension_name,
                    panel_id = %panel.id,
                    "Skipping panel with empty id or title"
                );
                return None;
            }
            let entry_point = resolve_asset_url(extension_name, &panel.entry_point).or_else(|| {
                warn!(
                    extension = extension_name,
                    panel_id = %panel.id,
                    entry_point = %panel.entry_point,
                    "Skipping panel with invalid entryPoint"
                );
                None
            })?;
            let icon = panel
                .icon
                .as_ref()
                .and_then(|icon| resolve_asset_url(extension_name, icon));
            Some(ResolvedPanel {
                extension_name: extension_name.to_owned(),
                id: format!("ext-{extension_name}-{}", panel.id),
                title: panel.title.clone(),
                entry_point,
                icon,
            })
        })
        .collect()
}

pub fn resolve_work_panel_views(
    views: &[ExtWorkPanelView],
    extension_name: &str,
    _ext_dir: &Path,
) -> Vec<ResolvedWorkPanelView> {
    views
        .iter()
        .filter_map(|view| {
            if view.id.is_empty() || view.title.is_empty() {
                warn!(
                    extension = extension_name,
                    view_id = %view.id,
                    "Skipping workPanelView with empty id or title"
                );
                return None;
            }
            let entry_point = resolve_asset_url(extension_name, &view.entry_point).or_else(|| {
                warn!(
                    extension = extension_name,
                    view_id = %view.id,
                    entry_point = %view.entry_point,
                    "Skipping workPanelView with invalid entryPoint"
                );
                None
            })?;
            let icon = view
                .icon
                .as_ref()
                .and_then(|icon| resolve_asset_url(extension_name, icon));
            Some(ResolvedWorkPanelView {
                extension_name: extension_name.to_owned(),
                id: format!("ext-{extension_name}-{}", view.id),
                title: view.title.clone(),
                entry_point,
                placement: view.placement.clone(),
                icon,
            })
        })
        .collect()
}

pub fn resolve_agent_tools(tools: &[ExtAgentTool], extension_name: &str) -> Vec<ResolvedAgentTool> {
    tools
        .iter()
        .filter_map(|tool| {
            if tool.id.is_empty() || tool.name.is_empty() {
                warn!(
                    extension = extension_name,
                    tool_id = %tool.id,
                    "Skipping agentTool with empty id or name"
                );
                return None;
            }
            Some(ResolvedAgentTool {
                extension_name: extension_name.to_owned(),
                id: format!("ext-{extension_name}-{}", tool.id),
                name: tool.name.clone(),
                description: tool.description.clone(),
                plan_safe_actions: tool.plan_safe_actions,
                handler: tool.handler.clone(),
            })
        })
        .collect()
}

pub fn resolve_background_services(
    services: &[ExtBackgroundService],
    extension_name: &str,
) -> Vec<ResolvedBackgroundService> {
    services
        .iter()
        .filter_map(|svc| {
            if svc.id.is_empty() || svc.entry_point.is_empty() {
                warn!(
                    extension = extension_name,
                    service_id = %svc.id,
                    "Skipping backgroundService with empty id or entryPoint"
                );
                return None;
            }
            let entry_point = resolve_asset_url(extension_name, &svc.entry_point).or_else(|| {
                // Allow opaque handler ids (non-path) for future spawners.
                if svc.entry_point.contains("://") || svc.entry_point.starts_with('/') {
                    warn!(
                        extension = extension_name,
                        service_id = %svc.id,
                        entry_point = %svc.entry_point,
                        "Skipping backgroundService with invalid entryPoint"
                    );
                    None
                } else {
                    Some(svc.entry_point.clone())
                }
            })?;
            Some(ResolvedBackgroundService {
                extension_name: extension_name.to_owned(),
                id: format!("ext-{extension_name}-{}", svc.id),
                entry_point,
                description: svc.description.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_skip_empty_and_prefix_id() {
        let cmds = vec![
            ExtCommand {
                id: "open".into(),
                title: "Open".into(),
                keybinding: Some("ctrl+o".into()),
                when: None,
            },
            ExtCommand {
                id: "".into(),
                title: "Bad".into(),
                keybinding: None,
                when: None,
            },
        ];
        let resolved = resolve_commands(&cmds, "demo.ext");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "ext-demo.ext-open");
        assert_eq!(resolved[0].keybinding.as_deref(), Some("ctrl+o"));
    }

    #[test]
    fn agent_tools_preserve_plan_safe_default_false() {
        let tools = vec![ExtAgentTool {
            id: "search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: false,
            handler: Some("handlers/search.js".into()),
        }];
        let resolved = resolve_agent_tools(&tools, "demo.ext");
        assert_eq!(resolved.len(), 1);
        assert!(!resolved[0].plan_safe_actions);
    }

    #[test]
    fn panels_resolve_asset_url() {
        let panels = vec![ExtPanel {
            id: "board".into(),
            title: "Board".into(),
            entry_point: "panels/board.html".into(),
            icon: None,
        }];
        let resolved = resolve_panels(&panels, "demo.ext", Path::new("/tmp"));
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].entry_point,
            "/api/extensions/demo.ext/assets/panels/board.html"
        );
    }
}
