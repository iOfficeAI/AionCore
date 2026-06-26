-- Migration 015: persist the built-in aionrs mode catalog.
--
-- aionrs is not an ACP backend, so it does not populate agent_metadata through
-- ACP handshake catalog sync. Pre-conversation surfaces such as Guid still read
-- mode options from agent_metadata via /api/agents/management, so seed the
-- stable aionrs runtime mode catalog here.

UPDATE agent_metadata
SET
    available_modes = '{
      "current_mode_id": "default",
      "available_modes": [
        { "id": "default", "name": "Default" },
        { "id": "auto_edit", "name": "Auto Edit" },
        { "id": "yolo", "name": "YOLO" }
      ]
    }',
    config_options = '{
      "config_options": [
        {
          "id": "mode",
          "name": "Mode",
          "category": "mode",
          "type": "select",
          "current_value": "default",
          "options": [
            { "value": "default", "name": "Default" },
            { "value": "auto_edit", "name": "Auto Edit" },
            { "value": "yolo", "name": "YOLO" }
          ]
        }
      ]
    }',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_type = 'aionrs'
  AND agent_source = 'internal';
