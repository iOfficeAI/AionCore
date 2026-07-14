use std::process::ExitCode;

use crate::cli::PrepareManagedResourcesArgs;
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};
use aionui_runtime::acp_tool_runtime::ManagedAcpToolId;
use aionui_runtime::acp_tool_runtime::managed_acp_tool_contract_for_export;
use aionui_runtime::managed_resources::export_node_runtime_to_root;
use aionui_runtime::managed_resources_contract::{
    MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION, ManagedResourcesContract, validate_contract, write_contract,
};
use aionui_runtime::node_runtime::managed_node_contract_for_export;
use aionui_runtime::{ensure_node_runtime, prepare_managed_acp_tool_to_root};

const SUBCOMMAND: &str = "prepare-managed-resources";

pub async fn run_prepare_managed_resources(args: PrepareManagedResourcesArgs) -> Result<ExitCode, CliBoundaryError> {
    let output_root = args.bundle_out;
    std::fs::create_dir_all(&output_root).map_err(|_| prepare_managed_resources_error("output.create"))?;

    let node_runtime = ensure_node_runtime()
        .await
        .map_err(|error| prepare_managed_resources_error_with_detail("node.prepare", error))?;
    let node_dir_name = node_runtime
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| prepare_managed_resources_error("node.layout"))?;
    let exported_node = export_node_runtime_to_root(&output_root, &node_runtime.root, node_dir_name)
        .map_err(|error| prepare_managed_resources_error_with_detail("node.export", error))?;

    println!("Prepared managed resources under {}", output_root.display());
    println!("  node   -> {}", exported_node.display());

    let mut prepared_tools = Vec::new();
    for tool in [ManagedAcpToolId::CodexAcp, ManagedAcpToolId::ClaudeAgentAcp] {
        let prepared = prepare_managed_acp_tool_to_root(tool, &output_root)
            .await
            .map_err(|error| prepare_managed_resources_error_with_detail("acp.prepare", error))?;
        println!("  {:<6} -> {}", tool.slug(), prepared.root.display());
        prepared_tools.push(prepared);
    }

    let node = managed_node_contract_for_export(&output_root, &exported_node)
        .map_err(|error| prepare_managed_resources_error_with_detail("contract.write", error))?;
    let mut acp_tools = Vec::new();
    for tool in [ManagedAcpToolId::CodexAcp, ManagedAcpToolId::ClaudeAgentAcp] {
        let prepared = prepared_tools
            .iter()
            .find(|prepared| prepared.id == tool)
            .ok_or_else(|| prepare_managed_resources_error("contract.write"))?;
        acp_tools.push(
            managed_acp_tool_contract_for_export(tool, &output_root, prepared)
                .map_err(|error| prepare_managed_resources_error_with_detail("contract.write", error))?,
        );
    }
    let runtime_key = acp_tools
        .first()
        .map(|tool| tool.platform_directory.clone())
        .ok_or_else(|| prepare_managed_resources_error("contract.write"))?;
    let contract = ManagedResourcesContract {
        schema_version: MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION,
        runtime_key,
        node,
        acp_tools,
    };
    let manifest_path = write_contract(&output_root, &contract)
        .map_err(|error| prepare_managed_resources_error_with_detail("contract.write", error))?;
    validate_contract(&output_root, &contract)
        .map_err(|error| prepare_managed_resources_error_with_detail("contract.validate", error))?;
    println!("  manifest -> {}", manifest_path.display());

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

fn prepare_managed_resources_error_with_detail(stage: &'static str, error: impl std::fmt::Display) -> CliBoundaryError {
    eprintln!("prepare-managed-resources stage={stage} detail: {error}");
    prepare_managed_resources_error(stage)
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

    #[test]
    fn prepare_error_accepts_contract_write_and_validate_stages() {
        for stage in ["contract.write", "contract.validate"] {
            let err = prepare_managed_resources_error(stage);
            assert_eq!(err.code(), CliBoundaryCode::CliPrepareManagedResourcesFailed);
            assert!(err.stderr_line().contains(stage));
        }
    }
}
