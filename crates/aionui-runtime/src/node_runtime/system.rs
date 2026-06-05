use std::path::{Path, PathBuf};

use super::types::{NodeRuntimeError, NodeTool, ResolvedCommand, ResolvedNodeRuntime};

pub fn derive_runtime_root(node: &Path, windows: bool) -> Option<PathBuf> {
    if windows {
        if node.file_name()?.to_str()? == "node.exe" {
            return node.parent().map(Path::to_path_buf);
        }
        return None;
    }

    let bin = node.parent()?;
    let root = bin.parent()?;
    (bin.file_name()?.to_str()? == "bin" && node.file_name()?.to_str()? == "node").then(|| root.to_path_buf())
}

pub fn validate_same_root(node: &Path, npm: &Path, npx: &Path) -> Result<(), NodeRuntimeError> {
    let canonical_node = std::fs::canonicalize(node).map_err(NodeRuntimeError::io_system)?;
    let canonical_npm = std::fs::canonicalize(npm).map_err(NodeRuntimeError::io_system)?;
    let canonical_npx = std::fs::canonicalize(npx).map_err(NodeRuntimeError::io_system)?;

    let node_root = derive_runtime_root(&canonical_node, cfg!(windows))
        .ok_or_else(|| NodeRuntimeError::system_invalid("cannot derive runtime root from node path"))?;

    if !canonical_npm.starts_with(&node_root) || !canonical_npx.starts_with(&node_root) {
        return Err(NodeRuntimeError::system_invalid(
            "npm/npx do not belong to the same runtime root as node",
        ));
    }

    Ok(())
}

pub fn tool_command(tool: NodeTool, runtime: &ResolvedNodeRuntime) -> ResolvedCommand {
    match tool {
        NodeTool::Node => ResolvedCommand::plain(runtime.node_path.clone()),
        NodeTool::Npm => runtime.npm_command(),
        NodeTool::Npx => runtime.npx_command(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_root_from_unix_bin_node() {
        let node = PathBuf::from("/opt/node-v24/bin/node");
        let root = derive_runtime_root(&node, false).expect("root");
        assert_eq!(root, PathBuf::from("/opt/node-v24"));
    }

    #[test]
    fn mixed_roots_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let node_root = root.path().join("node-a");
        let npm_root = root.path().join("node-b");

        std::fs::create_dir_all(node_root.join("bin")).unwrap();
        std::fs::create_dir_all(npm_root.join("bin")).unwrap();
        std::fs::write(node_root.join("bin/node"), b"").unwrap();
        std::fs::write(node_root.join("bin/npx"), b"").unwrap();
        std::fs::write(npm_root.join("bin/npm"), b"").unwrap();

        let err = validate_same_root(
            &node_root.join("bin/node"),
            &npm_root.join("bin/npm"),
            &node_root.join("bin/npx"),
        )
        .unwrap_err();

        assert!(err.to_string().contains("same runtime root"));
    }
}
