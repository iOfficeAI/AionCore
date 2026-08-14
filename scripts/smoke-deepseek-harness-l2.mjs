#!/usr/bin/env node
/**
 * Host-side DeepSeek Harness protocol helpers.
 *
 * These encode harness ideas we migrated into AionCore, without calling
 * DeepSeek or requiring DEEPSEEK_API_KEY:
 * - pick a real offered permission optionId (tools/pre-execute approval)
 * - recognize tool_call session updates (tools/post-execute / UI consume)
 *
 * Verified permission wire shape: crates/aionui-session/src/backend/acp_conn.rs
 * (`{ outcome: "selected", optionId }`).
 */

import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export function pickAllowOption(options) {
  if (!Array.isArray(options) || options.length === 0) {
    return null;
  }
  const byKind = (want) =>
    options.find((option) => option && typeof option === 'object' && option.kind === want && option.optionId);
  const chosen = byKind('allow_once') || byKind('allow_always') || options.find((option) => option?.optionId);
  return typeof chosen?.optionId === 'string' ? chosen.optionId : null;
}

export function isToolSessionUpdate(message) {
  const update = message?.params?.update;
  const kind = update?.sessionUpdate ?? update?.session_update;
  return kind === 'tool_call' || kind === 'tool_call_update';
}

export function permissionResponse(requestId, optionId) {
  return {
    jsonrpc: '2.0',
    id: requestId,
    result: optionId ? { outcome: 'selected', optionId } : { outcome: 'cancelled' },
  };
}

function fail(message) {
  console.error(`[deepseek-harness helpers] ${message}`);
  process.exit(1);
}

function selfCheck() {
  const allowOnce = pickAllowOption([
    { optionId: 'deny-1', kind: 'reject_once' },
    { optionId: 'allow-1', kind: 'allow_once' },
  ]);
  if (allowOnce !== 'allow-1') fail(`pickAllowOption should prefer allow_once, got ${allowOnce}`);
  if (pickAllowOption([]) !== null) fail('pickAllowOption should return null for empty options');
  if (
    !isToolSessionUpdate({
      method: 'session/update',
      params: { update: { sessionUpdate: 'tool_call', toolCallId: 't1' } },
    })
  ) {
    fail('isToolSessionUpdate should accept tool_call');
  }
  if (isToolSessionUpdate({ method: 'session/update', params: { update: { sessionUpdate: 'agent_message_chunk' } } })) {
    fail('isToolSessionUpdate should reject non-tool updates');
  }
  const selected = permissionResponse(9, 'allow-1');
  if (selected.result.outcome !== 'selected' || selected.result.optionId !== 'allow-1') {
    fail('permissionResponse should echo a real optionId');
  }
  console.log('[deepseek-harness helpers] self-check passed');
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  selfCheck();
}
