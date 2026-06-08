use std::process::ExitCode;

use crate::cli::PrepareManagedResourcesArgs;
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};
use aionui_runtime::acp_tool_runtime::ManagedAcpToolId;
use aionui_runtime::managed_resources::{export_acp_tool_to_root, export_node_runtime_to_root};
use aionui_runtime::{ensure_managed_acp_tool, ensure_node_runtime};

const SUBCOMMAND: &str = "prepare-managed-resources";

pub async fn run_prepare_managed_resources(args: PrepareManagedResourcesArgs) -> Result<ExitCode, CliBoundaryError> {
    let output_root = args.bundle_out;
    std::fs::create_dir_all(&output_root).map_err(|_| prepare_managed_resources_error("output.create"))?;

    let node_runtime = ensure_node_runtime()
        .await
        .map_err(|_| prepare_managed_resources_error("node.prepare"))?;
    let node_dir_name = node_runtime
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| prepare_managed_resources_error("node.layout"))?;
    let exported_node = export_node_runtime_to_root(&output_root, &node_runtime.root, node_dir_name)
        .map_err(|_| prepare_managed_resources_error("node.export"))?;

    println!("Prepared managed resources under {}", output_root.display());
    println!("  node   -> {}", exported_node.display());

    for tool in [ManagedAcpToolId::CodexAcp, ManagedAcpToolId::ClaudeAgentAcp] {
        let resolved = ensure_managed_acp_tool(tool)
            .await
            .map_err(|_| prepare_managed_resources_error("acp.prepare"))?;
        let platform = resolved
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| prepare_managed_resources_error("acp.layout"))?;
        let exported = export_acp_tool_to_root(&output_root, &resolved.root, tool.slug(), tool.version(), platform)
            .map_err(|_| prepare_managed_resources_error("acp.export"))?;
        println!("  {:<6} -> {}", tool.slug(), exported.display());
    }

    Ok(ExitCode::SUCCESS)
}

fn prepare_managed_resources_error(stage: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliPrepareManagedResourcesFailed,
        SUBCOMMAND,
        "failed to prepare managed resources",
    )
    .with_field("stage", stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_error_uses_stable_code_and_stage_without_raw_path() {
        let err = prepare_managed_resources_error("node.export");

        assert_eq!(err.code(), CliBoundaryCode::CliPrepareManagedResourcesFailed);
        assert!(err.stderr_line().starts_with(
            "CLI_PREPARE_MANAGED_RESOURCES_FAILED subcommand=prepare-managed-resources stage=node.export"
        ));
        assert!(!err.stderr_line().contains("/Users/secret/bundle"));
    }
}
