#!/usr/bin/env node
/**
 * Start the game server without blocking ExecCommand.
 *
 * `npm run play` is a long-lived Vite process. If the agent runs it in the
 * foreground, the tool times out (~120s) and the process group is killed.
 * This script detaches Vite, waits until http://127.0.0.1:5188/ answers,
 * then exits.
 *
 * Chapter QA / screenshots: `--no-open` (prints LAUNCH_OK, no browser).
 * Whole-game handoff: `--deliver` (prints GAME_DELIVERED and opens the
 * system default browser). Do not use `--deliver` after a single chapter.
 *
 * Run `npm install` as its own command first. This script does not install.
 */

import { spawn } from 'node:child_process';
import http from 'node:http';
import path from 'node:path';

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

function openSystemBrowser(url) {
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  delete env.ELECTRON_NO_ASAR;
  const opts = { detached: true, stdio: 'ignore', env, windowsHide: true };
  let child;
  if (process.platform === 'darwin') {
    child = spawn('open', [url], opts);
  } else if (process.platform === 'win32') {
    child = spawn('cmd.exe', ['/c', 'start', '', url], opts);
  } else {
    child = spawn('xdg-open', [url], opts);
  }
  child.unref();
}

function report(alreadyRunning, deliver) {
  const extra = alreadyRunning ? ' already_running=1' : '';
  if (deliver) {
    openSystemBrowser(URL);
    console.log(`GAME_DELIVERED url=${URL}${extra}`);
  } else {
    console.log(`LAUNCH_OK url=${URL}${extra}`);
  }
}

const { noOpen, deliver, target } = parseArgs(process.argv.slice(2));
const gameDir = path.resolve(target);
const inApp = Boolean(process.env.AIONUI_CDP_ACTIVE_PORT);
const skipViteOpen = noOpen || deliver || inApp;

if (await ping()) {
  report(true, deliver);
  process.exit(0);
}

startPlay(gameDir, skipViteOpen);
if (!(await waitReady())) {
  console.error(`Vite did not become ready at ${URL}. Run npm install in ${gameDir} first.`);
  process.exit(1);
}
report(false, deliver);
