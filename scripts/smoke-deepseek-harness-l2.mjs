#!/usr/bin/env node
/**
 * L2 cloud smoke: managed DeepSeek Harness goes online and completes one tool call.
 *
 * This is the ACP contract AionCore wraps after `POST /api/agents/{id}/runtime/prepare`:
 * spawn the installed entry, `initialize`, `session/new`, `session/prompt`,
 * auto-approve `session/request_permission` with a real offered optionId
 * (verified: crates/aionui-session/src/backend/acp_conn.rs).
 *
 * Without DEEPSEEK_API_KEY the live path exits 0 (skip). `--self-check`
 * always exercises the protocol helpers and does not call the network.
 */

import { execFileSync, spawn } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const runtimeDir = resolve(__dirname, '../crates/aionui-runtime/resources/deepseek-harness');
const npmShell = process.platform === 'win32';
const DEFAULT_MODEL = 'deepseek-chat';
const PROMPT_TEXT =
  'Use a filesystem or bash tool to list the files in the current workspace. Reply with only the file names.';

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
  console.error(`[deepseek-harness L2] ${message}`);
  process.exit(1);
}

function ok(message) {
  console.log(`[deepseek-harness L2] ${message}`);
}

function skip(message) {
  ok(`skipped: ${message}`);
  process.exit(0);
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
  ok('self-check passed');
}

function installRuntime(staging) {
  for (const name of ['package.json', 'package-lock.json', 'cordis.yml']) {
    cpSync(join(runtimeDir, name), join(staging, name));
  }
  execFileSync('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund', '--loglevel=error'], {
    cwd: staging,
    shell: npmShell,
    stdio: 'inherit',
    timeout: 300_000,
    env: { ...process.env, npm_config_fund: 'false', npm_config_audit: 'false' },
  });
}

function createAcpClient(child) {
  let nextId = 1;
  let buffer = '';
  const pending = new Map();

  const write = (payload) => {
    child.stdin.write(`${JSON.stringify(payload)}\n`);
  };

  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) !== -1) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        ok(`ignored non-JSON stdout line (${line.length} bytes)`);
        continue;
      }
      if (message.method === 'session/request_permission' && message.id != null) {
        const optionId = pickAllowOption(message.params?.options);
        ok(`auto-approving permission with optionId=${optionId ?? 'cancelled'}`);
        write(permissionResponse(message.id, optionId));
        continue;
      }
      if (message.id != null && pending.has(message.id)) {
        const { resolve, reject } = pending.get(message.id);
        pending.delete(message.id);
        if (message.error) reject(new Error(JSON.stringify(message.error)));
        else resolve(message);
        continue;
      }
      if (typeof client.onNotification === 'function' && message.method) {
        client.onNotification(message);
      }
    }
  });

  const client = {
    onNotification: null,
    request(method, params, timeoutMs) {
      const id = nextId++;
      write({ jsonrpc: '2.0', id, method, params });
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`${method} timed out after ${timeoutMs}ms`));
        }, timeoutMs);
        pending.set(id, {
          resolve: (value) => {
            clearTimeout(timer);
            resolve(value);
          },
          reject: (error) => {
            clearTimeout(timer);
            reject(error);
          },
        });
      });
    },
  };
  return client;
}

async function runLive() {
  const apiKey = process.env.DEEPSEEK_API_KEY?.trim();
  if (!apiKey) skip('DEEPSEEK_API_KEY is not set');

  const manifest = JSON.parse(readFileSync(join(runtimeDir, 'runtime-manifest.json'), 'utf8'));
  const staging = mkdtempSync(join(tmpdir(), 'aioncore-dsh-l2-'));
  const home = mkdtempSync(join(tmpdir(), 'aioncore-dsh-home-'));
  const sessions = mkdtempSync(join(tmpdir(), 'aioncore-dsh-sessions-'));
  const spill = mkdtempSync(join(tmpdir(), 'aioncore-dsh-spill-'));
  const workspace = mkdtempSync(join(tmpdir(), 'aioncore-dsh-ws-'));
  writeFileSync(join(workspace, 'README.md'), 'l2 smoke workspace\n');

  let child;
  try {
    ok(`installing managed runtime into ${staging}`);
    installRuntime(staging);
    const entryPath = join(staging, manifest.entry_path);
    const configPath = join(staging, manifest.config_path);
    if (!existsSync(entryPath)) fail(`entry missing after npm ci: ${manifest.entry_path}`);

    child = spawn(process.execPath, [entryPath, '--config', configPath], {
      cwd: workspace,
      env: {
        ...process.env,
        DEEPSEEK_API_KEY: apiKey,
        AIONUI_DSH_MODEL: process.env.AIONUI_DSH_MODEL?.trim() || DEFAULT_MODEL,
        AIONUI_DSH_HOME: home,
        AIONUI_DSH_SESSIONS_ROOT: sessions,
        AIONUI_DSH_SPILL_ROOT: spill,
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    const stderrChunks = [];
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      stderrChunks.push(chunk);
    });

    const died = new Promise((_, reject) => {
      child.on('exit', (code, signal) => {
        reject(new Error(`ACP process exited early (code=${code}, signal=${signal})`));
      });
    });

    const client = createAcpClient(child);
    let sawTool = false;
    client.onNotification = (message) => {
      if (message.method === 'session/update' && isToolSessionUpdate(message)) {
        sawTool = true;
        ok('received ACP tool session update');
      }
    };

    const initialize = await Promise.race([
      client.request(
        'initialize',
        {
          protocolVersion: 1,
          clientInfo: { name: 'CSBU WorkMate', version: 'l2-smoke' },
          clientCapabilities: { terminal: true, session: { configOptions: {} } },
        },
        30_000
      ),
      died,
    ]);
    if (initialize.result?.protocolVersion !== 1) {
      fail(`initialize protocolVersion is ${initialize.result?.protocolVersion}, expected 1`);
    }
    if (initialize.result?.agentCapabilities?.promptCapabilities?.image !== false) {
      fail('preview initialize must keep image prompt capability disabled');
    }
    ok('initialize completed');

    const created = await Promise.race([
      client.request('session/new', { cwd: workspace, mcpServers: [] }, 30_000),
      died,
    ]);
    const sessionId = created.result?.sessionId;
    if (!sessionId) fail('session/new did not return sessionId');
    ok('session/new completed');

    await Promise.race([
      client.request(
        'session/prompt',
        { sessionId, prompt: [{ type: 'text', text: PROMPT_TEXT }] },
        180_000
      ),
      died,
    ]);
    ok('session/prompt completed');

    if (!sawTool) {
      const stderr = stderrChunks.join('').slice(-2_000);
      fail(`session finished without a tool call${stderr ? `; stderr tail: ${stderr}` : ''}`);
    }
    ok('L2 smoke passed: agent online and completed one tool call');
  } finally {
    if (child && !child.killed) {
      child.kill('SIGTERM');
    }
    rmSync(staging, { recursive: true, force: true });
    rmSync(home, { recursive: true, force: true });
    rmSync(sessions, { recursive: true, force: true });
    rmSync(spill, { recursive: true, force: true });
    rmSync(workspace, { recursive: true, force: true });
  }
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  if (process.argv.includes('--self-check')) {
    selfCheck();
  } else {
    runLive().catch((error) => fail(error instanceof Error ? error.message : String(error)));
  }
}
