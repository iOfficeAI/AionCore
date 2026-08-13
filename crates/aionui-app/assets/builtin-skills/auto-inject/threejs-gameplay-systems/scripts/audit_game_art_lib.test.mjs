import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { auditGameArt } from './audit_game_art_lib.mjs';

function writeGame(files) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-art-audit-'));
  for (const [rel, content] of Object.entries(files)) {
    const dest = path.join(dir, rel);
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    fs.writeFileSync(dest, content);
  }
  return dir;
}

test('cone enemy in src/entities fails even if a markdown report claims PASS', () => {
  const dir = writeGame({
    'src/entities/Wraith.ts': 'const mesh = new THREE.ConeGeometry(0.42, 1.9, 10);\n',
    'src/entities/Player.ts': 'export class Player { constructor(private camera: unknown) {} }\n',
    'DELIVERY.md': 'Premium PASS. No primitives. All GLB present.\n',
    'public/look/look.json': JSON.stringify({ models: {} }),
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, false);
  assert.ok(result.failures.some((line) => /ConeGeometry/.test(line) && /Wraith\.ts/.test(line)));
});

test('missing look.json model file fails even when primitives are gone', () => {
  const dir = writeGame({
    'src/entities/Wraith.ts': 'export class Wraith {}\n',
    'src/entities/Player.ts': 'export class Player { constructor(private camera: unknown) {} }\n',
    'public/look/look.json': JSON.stringify({
      models: { enemy: { file: 'look/enemy.glb' } },
    }),
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, false);
  assert.ok(result.failures.some((line) => /enemy\.glb/.test(line) && /missing|not found/i.test(line)));
});

test('existing enemy and pickup models pass when source has no forbidden primitives', () => {
  const dir = writeGame({
    'src/entities/Wraith.ts': 'export class Wraith {}\n',
    'src/entities/Player.ts': 'export class Player { constructor(private camera: unknown) {} }\n',
    'src/entities/Pickup.ts': 'export class Pickup {}\n',
    'public/look/look.json': JSON.stringify({
      models: {
        enemy: { file: 'look/enemy.glb' },
        pickup: { file: 'look/pickup.glb' },
      },
    }),
    'public/look/enemy.glb': 'fake',
    'public/look/pickup.glb': 'fake',
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, true, result.failures.join('\n'));
  assert.deepEqual(result.failures, []);
});

test('Pickup.ts without a pickup model file fails deliver', () => {
  const dir = writeGame({
    'src/entities/Player.ts': 'export class Player { group = { add() {} }; }\n',
    'src/entities/Pickup.ts': 'export class Pickup {}\n',
    'public/look/look.json': JSON.stringify({
      models: { player: { file: 'look/player.fbx' } },
    }),
    'public/look/player.fbx': 'fake',
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, false);
  assert.ok(result.failures.some((line) => /pickup/i.test(line)));
});

test('an extra entity file requires an enemy model on disk', () => {
  const dir = writeGame({
    'src/entities/Player.ts': 'export class Player {}\n',
    'src/entities/Wraith.ts': 'export class Wraith {}\n',
    'public/look/look.json': JSON.stringify({
      models: { player: { file: 'look/player.fbx' } },
    }),
    'public/look/player.fbx': 'fake',
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, false);
  assert.ok(result.failures.some((line) => /enemy/i.test(line)));
});

test('CapsuleGeometry in Player without a player model file fails', () => {
  const dir = writeGame({
    'src/entities/Player.ts': 'const body = new THREE.CapsuleGeometry(0.28, 0.9, 4, 8);\n',
    'public/look/look.json': JSON.stringify({ models: {} }),
  });
  const result = auditGameArt(dir);
  assert.equal(result.ok, false);
  assert.ok(result.failures.some((line) => /CapsuleGeometry/.test(line)));
});
