-- Migration 029: Advertise session/delete for the Pi ACP builtin.
--
-- pi-acp v0.0.32 (svkozak/pi-acp#76) implements session/delete and advertises
-- agentCapabilities.sessionCapabilities.delete in the ACP handshake. The
-- release pin itself lives in crates/aionui-runtime/resources/acp-registry-npx-lock.json
-- (DB args stay unversioned: `["-y","pi-acp"]`; runtime injects @0.0.32).
-- Keep seeded agent_capabilities aligned with the live handshake so clients
-- that read metadata before probing still see the capability.

UPDATE agent_metadata
SET agent_capabilities = json_set(
        COALESCE(agent_capabilities, '{}'),
        '$.session_capabilities.delete',
        json('{}')
    ),
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_source = 'builtin'
  AND agent_type = 'acp'
  AND backend = 'pi'
  AND (
      agent_capabilities IS NULL
      OR json_extract(agent_capabilities, '$.session_capabilities.delete') IS NULL
  );
