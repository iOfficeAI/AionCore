import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { isToolSessionUpdate, permissionResponse, pickAllowOption } from './smoke-deepseek-harness-l2.mjs';

describe('pickAllowOption', () => {
  it('prefers an offered allow_once optionId', () => {
    assert.equal(
      pickAllowOption([
        { optionId: 'deny-1', kind: 'reject_once' },
        { optionId: 'allow-1', kind: 'allow_once' },
      ]),
      'allow-1'
    );
  });

  it('returns null when the agent offered no options', () => {
    assert.equal(pickAllowOption([]), null);
    assert.equal(pickAllowOption(undefined), null);
  });
});

describe('permissionResponse', () => {
  it('echoes a real optionId in the selected outcome', () => {
    assert.deepEqual(permissionResponse(4, 'allow-1'), {
      jsonrpc: '2.0',
      id: 4,
      result: { outcome: 'selected', optionId: 'allow-1' },
    });
  });

  it('cancels when no allow option exists', () => {
    assert.equal(permissionResponse(4, null).result.outcome, 'cancelled');
  });
});

describe('isToolSessionUpdate', () => {
  it('accepts ACP tool_call session updates', () => {
    assert.equal(
      isToolSessionUpdate({
        method: 'session/update',
        params: { update: { sessionUpdate: 'tool_call' } },
      }),
      true
    );
  });

  it('rejects assistant text chunks', () => {
    assert.equal(
      isToolSessionUpdate({
        method: 'session/update',
        params: { update: { sessionUpdate: 'agent_message_chunk' } },
      }),
      false
    );
  });
});
