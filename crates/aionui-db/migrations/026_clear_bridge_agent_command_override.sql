-- Bridge-launched rows (e.g. `npx`) keep the bridge's own arguments
-- (`-y <package> ...`) in `args`. A launch-path (command) override replaced the
-- bridge command with a bare binary path while retaining those bridge args,
-- producing broken invocations like `kilo.cmd -y @kilocode/cli acp` that fail
-- ACP initialization. Such overrides are never valid for bridge-launched rows,
-- so clear any already-persisted value. Only command_override is cleared;
-- env_override (API keys/proxies) and the base command stay intact.
UPDATE agent_metadata
SET command_override = NULL,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE command_override IS NOT NULL
  AND agent_source_info IS NOT NULL
  AND json_valid(agent_source_info)
  AND json_extract(agent_source_info, '$.bridge_binary') IS NOT NULL
  AND json_extract(agent_source_info, '$.bridge_binary') <> COALESCE(json_extract(agent_source_info, '$.binary_name'), '');
