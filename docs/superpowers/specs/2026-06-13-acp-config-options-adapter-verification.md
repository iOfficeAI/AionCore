# ACP Config Options Adapter Verification

Verification checklist for the observed-confirmation config option runtime path.

- HTTP exposes only `GET /api/conversations/{id}/config-options` and `PUT /api/conversations/{id}/config-options/{option_id}` for active ACP runtime config changes.
- Legacy HTTP `GET/PUT /mode` and `GET/PUT /model` routes are removed.
- Conversation service validates non-empty `option_id` and `value`, then forwards to the active agent task.
- Conversation service does not persist assistant preferences or `extra.current_*` from config option command ACKs.
- ACP manager uses a session-scoped in-flight guard and returns conflict while another config update is pending.
- ACP manager waits up to 10 seconds for observed state before returning success.
- `command_ack`, timeout, explicit command failure, and conflict do not self-apply observed runtime state.
- Real ACP `session/set_config_option` responses update the advertised config option snapshot, then observed confirmation is still required.
- Legacy ACP `session/set_mode` and `session/set_model` remain internal fallback paths only when the session has no real `config_options`.
- `model` and `thought_level` are independent config options; `current_model_id` remains a pure model id.
- Prompt injection model identity reminder is removed from the ACP prompt pipeline.
- Production-diagnostic logs exist for request, command ACK, observed confirmation, timeout, and in-flight conflict without prompt/tool payloads.
