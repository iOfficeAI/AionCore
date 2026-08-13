import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./create_threejs_game.mjs', import.meta.url));

test('new games get the overlay mixer, not the vendor oscillator-only file', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-create-game-'));
  const result = spawnSync(process.execPath, [script, dir, '--force'], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  const audio = fs.readFileSync(path.join(dir, 'src', 'systems', 'AudioSystem.ts'), 'utf8');
  assert.match(audio, /\/audio\/kit\.json/);
  assert.match(audio, /setMusic/);
  assert.match(audio, /voice:settle|cue === 'settle'|cue == 'settle'/);
  assert.match(audio, /eventsFromDiagnostics|player\.speed|dashing/);
  assert.match(audio, /playVoice|voice\/line/);
  assert.doesNotMatch(audio, /bodyGeometry/);

  const player = fs.readFileSync(path.join(dir, 'src', 'entities', 'Player.ts'), 'utf8');
  assert.doesNotMatch(player, /CapsuleGeometry/);
  assert.doesNotMatch(player, /ConeGeometry/);
  assert.match(player, /torso|cloak|emblem/i);
  assert.match(player, /applyCast|loadCastVisual/);

  const pickup = fs.readFileSync(path.join(dir, 'src', 'entities', 'Pickup.ts'), 'utf8');
  assert.doesNotMatch(pickup, /IcosahedronGeometry/);
  assert.match(pickup, /applyCast|loadCastVisual/);

  const cast = fs.readFileSync(path.join(dir, 'src', 'studio', 'cast.ts'), 'utf8');
  assert.match(cast, /Root\.position/);
  assert.match(cast, /FBXLoader/);
  assert.doesNotMatch(cast, /animate-in-place|animateInPlace/);

  const game = fs.readFileSync(path.join(dir, 'src', 'game', 'Game.ts'), 'utf8');
  assert.match(game, /chapterFromScore|diagnostics\.chapter/);
  assert.match(game, /PauseShare|pause-overlay/);
  assert.match(game, /LookSystem|\/look\/look\.json/);

  const html = fs.readFileSync(path.join(dir, 'index.html'), 'utf8');
  assert.match(html, /pause-overlay/);
  assert.match(html, /share-button/);
  assert.match(html, /settle-overlay/);

  const look = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'look', 'look.json'), 'utf8'));
  assert.equal(look.chapters.length, 3);
  assert.equal(look.cartridge, 'collect');
  assert.match(game, /createWorldKit|WorldKit/);
  assert.match(game, /cartridge === 'jump'|cartridge !== 'jump'/);
  assert.equal(fs.existsSync(path.join(dir, 'src', 'systems', 'WorldKit.ts')), true);
  assert.equal(fs.existsSync(path.join(dir, 'src', 'core', 'InputController.ts')), true);
});

test('jump cartridge writes look.json without replacing overlay Game.ts', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-create-jump-'));
  const result = spawnSync(process.execPath, [script, dir, '--force', '--cartridge', 'jump'], {
    encoding: 'utf8',
  });
  assert.equal(result.status, 0, result.stderr);
  const look = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'look', 'look.json'), 'utf8'));
  assert.equal(look.cartridge, 'jump');
  const game = fs.readFileSync(path.join(dir, 'src', 'game', 'Game.ts'), 'utf8');
  assert.match(game, /createWorldKit/);
});
