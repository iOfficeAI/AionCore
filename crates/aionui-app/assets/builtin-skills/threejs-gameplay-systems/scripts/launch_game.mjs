#!/usr/bin/env node
/**
 * Start the game server without blocking ExecCommand.
 *
 * `npm run play` is a long-lived Vite process. If the agent runs it in the
 * foreground, the tool times out (~120s) and the process group is killed.
 * This script detaches `npm run play` (which already opens the browser),
 * waits until http://127.0.0.1:5188/ answers, then exits.
 *
 * Run `npm install` as its own command first. This script does not install.
 */

import { spawn } from 'node:child_process';
import http from 'node:http';
import path from 'node:path';

const URL = 'http://127.0.0.1:5188/';
const READY_MS = 20_000;

function parseArgs(argv) {
  let noOpen = false;
  let target = null;
  for (const arg of argv) {
    if (arg === '--no-open') noOpen = true;
    else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node launch_game.mjs <game-dir> [--no-open]');
      process.exit(0);
    } else if (arg.startsWith('-')) {
      throw new Error(`unknown flag: ${arg}`);
    } else if (target) {
      throw new Error('Usage: node launch_game.mjs <game-dir> [--no-open]');
    } else {
      target = arg;
    }
  }
  if (!target) {
    throw new Error('Usage: node launch_game.mjs <game-dir> [--no-open]');
  }
  return { noOpen, target };
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

function startPlay(gameDir, noOpen) {
  const child = spawn('npm', noOpen ? ['run', 'dev'] : ['run', 'play'], {
    cwd: gameDir,
    detached: true,
    stdio: 'ignore',
    shell: process.platform === 'win32',
    windowsHide: true,
    env: process.env,
  });
  child.unref();
}

const { noOpen, target } = parseArgs(process.argv.slice(2));
const gameDir = path.resolve(target);

if (await ping()) {
  console.log(`LAUNCH_OK url=${URL} already_running=1`);
  process.exit(0);
}

startPlay(gameDir, noOpen);
if (!(await waitReady())) {
  console.error(`Vite did not become ready at ${URL}. Run npm install in ${gameDir} first.`);
  process.exit(1);
}
console.log(`LAUNCH_OK url=${URL}`);
