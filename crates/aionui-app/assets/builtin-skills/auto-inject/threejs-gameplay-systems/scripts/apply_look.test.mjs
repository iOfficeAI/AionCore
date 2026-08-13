import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./apply_look.mjs', import.meta.url));

test('apply_look copies generated images into public/look and writes look.json', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-look-'));
  const sky = path.join(dir, 'sky-src.png');
  const ground = path.join(dir, 'ground-src.png');
  const icon = path.join(dir, 'icon-src.png');
  fs.writeFileSync(sky, 'sky');
  fs.writeFileSync(ground, 'ground');
  fs.writeFileSync(icon, 'icon');

  const result = spawnSync(
    process.execPath,
    [script, '--out', dir, '--sky', sky, '--ground', ground, '--icon', icon, '--title', 'Lantern ferry'],
    { encoding: 'utf8' },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(path.join(dir, 'public', 'look', 'sky.png')), true);
  assert.equal(fs.existsSync(path.join(dir, 'public', 'look', 'ground.png')), true);
  assert.equal(fs.existsSync(path.join(dir, 'public', 'look', 'icon.png')), true);
  const look = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'look', 'look.json'), 'utf8'));
  assert.equal(look.title, 'Lantern ferry');
  assert.equal(look.look.sky, 'look/sky.png');
  assert.doesNotMatch(JSON.stringify(look), /concepts/);
});

test('apply_look copies cast models into public/look and records slots', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-look-models-'));
  const player = path.join(dir, 'idle.fbx');
  const pickup = path.join(dir, 'mark.glb');
  fs.writeFileSync(player, 'fbx');
  fs.writeFileSync(pickup, 'glb');

  const result = spawnSync(
    process.execPath,
    [script, '--out', dir, '--player', player, '--pickup', pickup],
    { encoding: 'utf8' },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(path.join(dir, 'public', 'look', 'player.fbx')), true);
  assert.equal(fs.existsSync(path.join(dir, 'public', 'look', 'pickup.glb')), true);
  const look = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'look', 'look.json'), 'utf8'));
  assert.equal(look.models.player.file, 'look/player.fbx');
  assert.equal(look.models.pickup.file, 'look/pickup.glb');
});
