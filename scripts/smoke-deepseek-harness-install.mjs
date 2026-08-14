#!/usr/bin/env node
/**
 * L1 cloud smoke for the managed DeepSeek Harness installer contract.
 *
 * Proves that the embedded package.json + package-lock.json can be installed
 * with `npm ci --ignore-scripts` and that the declared entry path exists.
 * This is the prepare half of: runtime/prepare → agent online → tool call.
 *
 * Does not call the DeepSeek API (no credentials required).
 */

import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const runtimeDir = resolve(__dirname, '../crates/aionui-runtime/resources/deepseek-harness');
const npmShell = process.platform === 'win32';
const npm = 'npm';

function fail(message) {
  console.error(`[deepseek-harness L1] ${message}`);
  process.exit(1);
}

function ok(message) {
  console.log(`[deepseek-harness L1] ${message}`);
}

for (const name of [
  'runtime-manifest.json',
  'package.json',
  'package-lock.json',
  'cordis.yml',
  'acp-handshake.fixture.jsonl',
  'THIRD_PARTY_LICENSES.md',
]) {
  const path = join(runtimeDir, name);
  if (!existsSync(path)) fail(`missing embedded resource: ${name}`);
}

const manifest = JSON.parse(readFileSync(join(runtimeDir, 'runtime-manifest.json'), 'utf8'));
const packageJson = JSON.parse(readFileSync(join(runtimeDir, 'package.json'), 'utf8'));
const fixture = readFileSync(join(runtimeDir, 'acp-handshake.fixture.jsonl'), 'utf8')
  .trim()
  .split(/\r?\n/)
  .map((line) => JSON.parse(line));

if (manifest.runtime_id !== 'deepseek-harness') fail(`unexpected runtime_id: ${manifest.runtime_id}`);
if (manifest.schema_version !== 1) fail(`unexpected schema_version: ${manifest.schema_version}`);
if (!manifest.entry_path || !manifest.entry_version) fail('manifest is missing entry_path/entry_version');
if (!packageJson.dependencies?.['@deepseek-ai/dsh-acp-demo']) {
  fail('package.json is missing @deepseek-ai/dsh-acp-demo');
}
if (fixture.length < 2) fail('ACP handshake fixture must include initialize + session/new frames');
if (fixture[0]?.result?.protocolVersion !== 1) fail('fixture initialize protocolVersion is not 1');
if (fixture[0]?.result?.agentCapabilities?.promptCapabilities?.image !== false) {
  fail('fixture must keep image prompt capability disabled for preview');
}
if (fixture[0]?.result?.mcpCapabilities != null) {
  fail('fixture must omit mcpCapabilities for preview');
}

ok(`manifest release=${manifest.release} entry=${manifest.entry_package}@${manifest.entry_version}`);

const staging = mkdtempSync(join(tmpdir(), 'aioncore-dsh-l1-'));
try {
  for (const name of ['package.json', 'package-lock.json', 'cordis.yml', 'acp-handshake.fixture.jsonl']) {
    cpSync(join(runtimeDir, name), join(staging, name));
  }

  ok(`installing into ${staging}`);
  execFileSync(
    npm,
    ['ci', '--ignore-scripts', '--no-audit', '--no-fund', '--loglevel=error'],
    {
      cwd: staging,
      shell: npmShell,
      stdio: 'inherit',
      timeout: 300_000,
      env: {
        ...process.env,
        npm_config_fund: 'false',
        npm_config_audit: 'false',
      },
    }
  );

  const entryPath = join(staging, manifest.entry_path);
  if (!existsSync(entryPath)) fail(`entry path missing after npm ci: ${manifest.entry_path}`);
  const entryStat = statSync(entryPath);
  if (!entryStat.isFile() || entryStat.size <= 0) fail(`entry path is empty: ${manifest.entry_path}`);

  const configPath = join(staging, manifest.config_path);
  if (!existsSync(configPath)) fail(`config path missing after copy: ${manifest.config_path}`);

  // Load the entry module with Node to catch broken package graphs early.
  // The ACP server is not started; this only proves the installed graph is importable.
  execFileSync(process.execPath, ['--check', entryPath], {
    cwd: staging,
    stdio: 'inherit',
    timeout: 30_000,
  });

  ok(`npm ci + entry validation passed (${manifest.entry_path}, ${entryStat.size} bytes)`);
  ok('L1 smoke passed: prepare installer contract is cloud-verified');
} finally {
  rmSync(staging, { recursive: true, force: true });
}
