#!/usr/bin/env node
/**
 * Start the game server without blocking ExecCommand, or hand off a built dist.
 *
 * Chapter QA / screenshots: `--no-open` detaches Vite and prints LAUNCH_OK.
 * Whole-game handoff: `--deliver` audits art, runs `npm run build`, and prints
 * `GAME_DELIVERED dist=<abs>/dist`. It does not start Vite, occupy a port, or
 * open a system browser — AionUi mounts that dist in the in-app preview.
 *
 * Run `npm install` as its own command first. This script does not install.
 */

import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { auditGameArt } from './audit_game_art_lib.mjs';

const URL = 'http://127.0.0.1:5188/';
const READY_MS = 20_000;
const USAGE = 'Usage: node launch_game.mjs <game-dir> [--no-open|--deliver]';

function parseArgs(argv) {
  let noOpen = false;
  let deliver = false;
  let target = null;
  for (const arg of argv) {
    if (arg === '--no-open') noOpen = true;
    else if (arg === '--deliver') deliver = true;
    else if (arg === '--help' || arg === '-h') {
      console.log(USAGE);
      process.exit(0);
    } else if (arg.startsWith('-')) {
      throw new Error(`unknown flag: ${arg}`);
    } else if (target) {
      throw new Error(USAGE);
    } else {
      target = arg;
    }
  }
  if (!target) {
    throw new Error(USAGE);
  }
  if (noOpen && deliver) {
    throw new Error('use either --no-open or --deliver, not both');
  }
  return { noOpen, deliver, target };
}

function ping() {
  return new Promise((resolve) => {
    const req = http.get(URL, { timeout: 800 }, (res) => {
      res.resume();
      resolve(true);
    });
    req.on('timeout', () => {
      req.destroy();
      resolve(false);
    });
    req.on('error', () => resolve(false));
  });
}

async function waitReady() {
  const deadline = Date.now() + READY_MS;
  while (Date.now() < deadline) {
    if (await ping()) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
}

function startPlay(gameDir, skipViteOpen) {
  const child = spawn('npm', skipViteOpen ? ['run', 'dev'] : ['run', 'play'], {
    cwd: gameDir,
    detached: true,
    stdio: 'ignore',
    shell: process.platform === 'win32',
    windowsHide: true,
    env: process.env,
  });
  child.unref();
}

function runBuild(gameDir) {
  const result = spawnSync('npm', ['run', 'build'], {
    cwd: gameDir,
    encoding: 'utf8',
    shell: process.platform === 'win32',
    env: process.env,
  });
  if (result.status !== 0) {
    if (result.stdout) console.error(result.stdout);
    if (result.stderr) console.error(result.stderr);
    throw new Error(`npm run build failed in ${gameDir}`);
  }
  const distDir = path.join(gameDir, 'dist');
  const index = path.join(distDir, 'index.html');
  if (!fs.existsSync(index)) {
    throw new Error(`build did not write ${index}`);
  }
  return distDir;
}

const { noOpen, deliver, target } = parseArgs(process.argv.slice(2));
const gameDir = path.resolve(target);

if (deliver) {
  const art = auditGameArt(gameDir);
  if (!art.ok) {
    for (const line of art.failures) console.error(`ART_FAIL ${line}`);
    process.exit(2);
  }
  try {
    const distDir = runBuild(gameDir);
    console.log(`GAME_DELIVERED dist=${distDir}`);
    process.exit(0);
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}

const inApp = Boolean(process.env.AIONUI_CDP_ACTIVE_PORT);
const skipViteOpen = noOpen || inApp;

if (await ping()) {
  console.log(`LAUNCH_OK url=${URL} already_running=1`);
  process.exit(0);
}

startPlay(gameDir, skipViteOpen);
if (!(await waitReady())) {
  console.error(`Vite did not become ready at ${URL}. Run npm install in ${gameDir} first.`);
  process.exit(1);
}
console.log(`LAUNCH_OK url=${URL}`);
