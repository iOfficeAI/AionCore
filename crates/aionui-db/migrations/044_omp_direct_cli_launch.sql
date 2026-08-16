-- omp launches its local CLI directly instead of bridging through npx.
--
-- 031 seeded omp as `npx -y @oh-my-pi/pi-coding-agent acp` with
-- `binary_name: "omp"`, following the shape used by the Registry-listed npx
-- rows. That shape does not fit omp:
--
--   * The Registry rows bridge because the ACP Registry declares an npx
--     distribution for them (verified against the public catalogue: 11 of the
--     13 npx rows match their Registry entry's package and args exactly). omp
--     is NOT listed on the Registry, so there is no declared distribution to
--     conform to — the bridge was chosen by analogy, not by evidence.
--   * `@oh-my-pi/pi-coding-agent` is not an ACP adapter wrapping some other
--     CLI; it ships bin `omp` (package.json `bin: {omp: dist/cli.js}`) and
--     `omp acp` is the vendor's own entrypoint. The package IS the product.
--   * `binary_name: "omp"` already made a local `omp` on $PATH mandatory:
--     `probe_resolved_command` fails the row with `PrimaryMissing` without it,
--     and `cli_probe::validate_with_budget` already runs the local
--     `omp --version`. So the bridge re-downloaded a CLI the user had to have
--     installed before the row was even offered.
--
-- The cost of that was measured, spawn to `initialize` response: 81.0s on a
-- cold npx cache and 10.0s warm, against 0.7s for the local binary. The cold
-- figure is from a fast link and exceeded the 30s handshake budget outright,
-- which is the failure reported in iOfficeAI/AionUi#4009.
--
-- Written as an UPDATE rather than a re-seed on purpose: `agent_capabilities`
-- and `auth_methods` hold what a live handshake learned on THIS install, and
-- an `ON CONFLICT DO UPDATE` that lists them resets that to an integration-time
-- snapshot. An UPDATE of the launch columns cannot reach them at all.
--
-- Version pinning moves with the launch path: the `acp-registry-npx-lock.json`
-- entry is dropped in the same change, so omp now tracks whatever the user has
-- installed, exactly as the other direct-CLI builtins (qwen, agy) do.
UPDATE agent_metadata
SET command = 'omp',
    args = '["acp"]',
    agent_source_info = '{"binary_name":"omp"}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_source = 'builtin'
  AND backend = 'omp';
