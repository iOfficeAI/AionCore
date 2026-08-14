#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const runtimeDir = resolve('crates/aionui-runtime/resources/deepseek-harness');
const npm = 'npm';
const npmShell = process.platform === 'win32';
const current = JSON.parse(readFileSync(join(runtimeDir, 'package.json'), 'utf8'));
const packageName = '@deepseek-ai/dsh-acp-demo';
const installed = current.dependencies[packageName];
const candidate =
  process.argv[2] ||
  execFileSync(npm, ['view', packageName, 'version'], {
    encoding: 'utf8',
    shell: npmShell,
    timeout: 30_000,
  }).trim();

if (candidate === installed) {
  console.log(`DeepSeek Harness runtime is current: ${installed}`);
  process.exit(0);
}

const staging = mkdtempSync(join(tmpdir(), 'aionui-dsh-candidate-'));
try {
  const next = structuredClone(current);
  for (const name of Object.keys(next.dependencies)) {
    if (name.startsWith('@deepseek-ai/')) next.dependencies[name] = candidate;
  }
  writeFileSync(join(staging, 'package.json'), `${JSON.stringify(next, null, 2)}\n`);
  execFileSync(npm, ['install', '--package-lock-only', '--ignore-scripts', '--no-audit', '--no-fund'], {
    cwd: staging,
    shell: npmShell,
    stdio: 'inherit',
    timeout: 120_000,
  });
  const output = resolve(`deepseek-harness-${candidate}-candidate-lock.json`);
  writeFileSync(output, readFileSync(join(staging, 'package-lock.json')));
  console.log(`Candidate ${candidate} lock written to ${output}`);
  console.log('Review upstream commit, licenses, Cordis config diff and ACP fixtures before changing the embedded manifest.');
} finally {
  rmSync(staging, { recursive: true, force: true });
}
