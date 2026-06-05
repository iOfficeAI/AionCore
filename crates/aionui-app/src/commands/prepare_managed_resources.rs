use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::cli::PrepareManagedResourcesArgs;
use aionui_runtime::acp_tool_runtime::ManagedAcpToolId;
use aionui_runtime::managed_resources::{export_acp_tool_to_root, export_node_runtime_to_root};
use aionui_runtime::{ensure_managed_acp_tool, ensure_node_runtime};

pub async fn run_prepare_managed_resources(args: PrepareManagedResourcesArgs) -> Result<ExitCode> {
    let output_root = args.bundle_out;
    std::fs::create_dir_all(&output_root)
        .with_context(|| format!("create bundle output root {}", output_root.display()))?;

    let node_runtime = ensure_node_runtime().await.context("prepare managed Node runtime")?;
    let node_dir_name = node_runtime
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .context("managed Node runtime root missing directory name")?;
    let exported_node = export_node_runtime_to_root(&output_root, &node_runtime.root, node_dir_name)
        .context("export managed Node runtime to bundle root")?;

    println!("Prepared managed resources under {}", output_root.display());
    println!("  node   -> {}", exported_node.display());

    for tool in [ManagedAcpToolId::CodexAcp, ManagedAcpToolId::ClaudeAgentAcp] {
        let resolved = ensure_managed_acp_tool(tool)
            .await
            .with_context(|| format!("prepare managed {} artifact", tool.display_name()))?;
        let platform = resolved
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .context("managed ACP tool root missing platform directory name")?;
        let exported = export_acp_tool_to_root(&output_root, &resolved.root, tool.slug(), tool.version(), platform)
            .with_context(|| format!("export managed {} artifact to bundle root", tool.display_name()))?;
        println!("  {:<6} -> {}", tool.slug(), exported.display());
    }

    Ok(ExitCode::SUCCESS)
}
