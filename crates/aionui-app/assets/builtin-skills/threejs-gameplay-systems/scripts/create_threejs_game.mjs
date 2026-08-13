#!/usr/bin/env node
/** Create a Three.js Vite game from the packaged skill scaffold. Node twin of create_threejs_game.py. */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { withCartridgeSession } from './studio_session_lib.mjs';

const EXCLUDE_DIRS = new Set([
  'node_modules',
  'dist',
  'artifacts',
  'test-results',
  'playwright-report',
  'coverage',
  '__pycache__',
]);
const EXCLUDE_FILES = new Set(['.DS_Store']);

function skillDir() {
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
}

function scaffoldDir() {
  return path.join(skillDir(), 'assets', 'threejs-vite-game');
}

function overlayDir() {
  return path.join(skillDir(), 'assets', 'aion-overlay');
}

function normalizedProjectName(target) {
  const name = path
    .basename(path.resolve(target))
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return name || 'threejs-vite-game';
}

function copyTree(source, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const entry of fs.readdirSync(source, { withFileTypes: true })) {
    if (EXCLUDE_DIRS.has(entry.name) || EXCLUDE_FILES.has(entry.name)) {
      continue;
    }
    const from = path.join(source, entry.name);
    const to = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyTree(from, to);
    } else if (entry.isFile()) {
      fs.copyFileSync(from, to);
    }
  }
}

function rewriteJsonName(filePath, name) {
  if (!fs.existsSync(filePath)) {
    return;
  }
  const data = JSON.parse(fs.readFileSync(filePath, 'utf8'));
  data.name = name;
  if (data.packages && typeof data.packages === 'object' && '' in data.packages) {
    data.packages[''].name = name;
  }
  fs.writeFileSync(filePath, `${JSON.stringify(data, null, 2)}\n`);
}

function createGame(target, force, cartridge) {
  const source = scaffoldDir();
  if (!fs.existsSync(source) || !fs.statSync(source).isDirectory()) {
    throw new Error(`Scaffold not found: ${source}`);
  }

  if (fs.existsSync(target) && fs.readdirSync(target).length > 0 && !force) {
    throw new Error(`Target is not empty: ${target}\nUse --force to copy into it anyway.`);
  }

  fs.mkdirSync(target, { recursive: true });
  copyTree(source, target);
  applyAionOverlay(target);
  writeCartridgeSession(target, cartridge);

  const projectName = normalizedProjectName(target);
  rewriteJsonName(path.join(target, 'package.json'), projectName);
  rewriteJsonName(path.join(target, 'package-lock.json'), projectName);

  console.log(`Created Three.js game scaffold at ${path.resolve(target)}`);
  console.log(`Cartridge: ${cartridge}`);
  console.log(`Next: cd ${path.resolve(target)} && npm install && node <gameplay-skill>/scripts/launch_game.mjs .`);
}

function writeCartridgeSession(target, cartridge) {
  const kitPath = path.join(target, 'public', 'look', 'look.json');
  const current = fs.existsSync(kitPath)
    ? JSON.parse(fs.readFileSync(kitPath, 'utf8'))
    : {};
  const session = withCartridgeSession(current, cartridge);
  fs.mkdirSync(path.dirname(kitPath), { recursive: true });
  fs.writeFileSync(kitPath, `${JSON.stringify(session, null, 2)}\n`);
}

function applyAionOverlay(target) {
  const overlay = overlayDir();
  if (!fs.existsSync(overlay)) return;
  copyTree(overlay, target);
}

function parseArgs(argv) {
  let force = false;
  let cartridge = 'collect';
  const rest = [];
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--force') {
      force = true;
    } else if (arg === '--help' || arg === '-h') {
      console.log('Create a Vite + TypeScript + Three.js browser game scaffold.\n');
      console.log('Usage: node create_threejs_game.mjs <target> [--force] [--cartridge collect|jump]');
      process.exit(0);
    } else if (arg === '--cartridge') {
      cartridge = argv[i + 1] || '';
      i += 1;
    } else if (arg.startsWith('--cartridge=')) {
      cartridge = arg.slice('--cartridge='.length);
    } else if (arg.startsWith('-')) {
      throw new Error('Usage: node create_threejs_game.mjs <target> [--force] [--cartridge collect|jump]');
    } else {
      rest.push(arg);
    }
  }
  if (rest.length !== 1) {
    throw new Error('Usage: node create_threejs_game.mjs <target> [--force] [--cartridge collect|jump]');
  }
  if (cartridge !== 'collect' && cartridge !== 'jump') {
    throw new Error('cartridge must be collect or jump');
  }
  return { target: rest[0], force, cartridge };
}

try {
  const { target, force, cartridge } = parseArgs(process.argv.slice(2));
  createGame(target, force, cartridge);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
