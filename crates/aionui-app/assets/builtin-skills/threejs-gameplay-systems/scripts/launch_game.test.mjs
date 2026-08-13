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
