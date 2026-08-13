import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./launch_game.mjs', import.meta.url));

test('--deliver refuses a cone enemy and never prints GAME_DELIVERED', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-deliver-art-'));
  fs.mkdirSync(path.join(dir, 'src', 'entities'), { recursive: true });
  fs.mkdirSync(path.join(dir, 'public', 'look'), { recursive: true });
  fs.writeFileSync(
    path.join(dir, 'src', 'entities', 'Wraith.ts'),
    'const mesh = new THREE.ConeGeometry(0.42, 1.9, 10);\n',
  );
  fs.writeFileSync(
    path.join(dir, 'public', 'look', 'look.json'),
    JSON.stringify({ models: {} }),
  );

  const result = spawnSync(process.execPath, [script, dir, '--deliver'], { encoding: 'utf8' });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /ART_FAIL/);
  assert.match(result.stderr, /ConeGeometry/);
  assert.doesNotMatch(result.stdout, /GAME_DELIVERED/);
});

test('--deliver builds dist and prints GAME_DELIVERED dist= without opening a port', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-deliver-dist-'));
  fs.mkdirSync(path.join(dir, 'src', 'entities'), { recursive: true });
  fs.mkdirSync(path.join(dir, 'public', 'look'), { recursive: true });
  fs.writeFileSync(
    path.join(dir, 'src', 'entities', 'Player.ts'),
    'export class Player {}\n',
  );
  fs.writeFileSync(
    path.join(dir, 'public', 'look', 'look.json'),
    JSON.stringify({ models: { player: { file: 'look/player.glb' } } }),
  );
  fs.writeFileSync(path.join(dir, 'public', 'look', 'player.glb'), 'fake');
  fs.writeFileSync(
    path.join(dir, 'package.json'),
    JSON.stringify({
      name: 'deliver-dist-game',
      scripts: {
        build: "node -e \"require('fs').mkdirSync('dist',{recursive:true}); require('fs').writeFileSync('dist/index.html','ok')\"",
      },
    }),
  );

  const result = spawnSync(process.execPath, [script, dir, '--deliver'], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /GAME_DELIVERED dist=/);
  assert.doesNotMatch(result.stdout, /url=http:\/\/127\.0\.0\.1:5188/);
  assert.doesNotMatch(result.stdout, /already_running/);
  const delivered = /GAME_DELIVERED dist=(\S+)/.exec(result.stdout);
  assert.ok(delivered);
  assert.equal(delivered[1], path.join(dir, 'dist'));
  assert.equal(fs.existsSync(path.join(dir, 'dist', 'index.html')), true);
});
