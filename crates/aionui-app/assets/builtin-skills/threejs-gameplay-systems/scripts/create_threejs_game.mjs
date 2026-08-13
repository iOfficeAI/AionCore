#!/usr/bin/env node
/** Create a Three.js Vite game from the packaged skill scaffold. Node twin of create_threejs_game.py. */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

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

function createGame(target, force) {
  const source = scaffoldDir();
  if (!fs.existsSync(source) || !fs.statSync(source).isDirectory()) {
    throw new Error(`Scaffold not found: ${source}`);
  }

  if (fs.existsSync(target) && fs.readdirSync(target).length > 0 && !force) {
    throw new Error(`Target is not empty: ${target}\nUse --force to copy into it anyway.`);
  }

  fs.mkdirSync(target, { recursive: true });
  copyTree(source, target);

  const projectName = normalizedProjectName(target);
  rewriteJsonName(path.join(target, 'package.json'), projectName);
  rewriteJsonName(path.join(target, 'package-lock.json'), projectName);

  console.log(`Created Three.js game scaffold at ${path.resolve(target)}`);
  console.log(`Next: cd ${path.resolve(target)} && npm install && node <gameplay-skill>/scripts/launch_game.mjs .`);
}

function parseArgs(argv) {
  let force = false;
  const rest = [];
  for (const arg of argv) {
    if (arg === '--force') {
      force = true;
    } else if (arg === '--help' || arg === '-h') {
      console.log('Create a Vite + TypeScript + Three.js browser game scaffold.\n');
      console.log('Usage: node create_threejs_game.mjs <target> [--force]');
      process.exit(0);
    } else {
      rest.push(arg);
    }
  }
  if (rest.length !== 1) {
    throw new Error('Usage: node create_threejs_game.mjs <target> [--force]');
  }
  return { target: rest[0], force };
}

try {
  const { target, force } = parseArgs(process.argv.slice(2));
  createGame(target, force);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
