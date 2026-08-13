import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  chapterFromScore,
  defaultSession,
  lookPaths,
  mergeCastModels,
  modelLookPaths,
  normalizeCartridge,
  pickupLayout,
  platformLayout,
  scaleChapters,
  sharePayload,
  withCartridgeSession,
} from './studio_session_lib.mjs';

test('default session has three named chapters that cover the full score', () => {
  const session = defaultSession();
  assert.equal(session.chapters.length, 3);
  assert.equal(session.chapters[0].id, 'explore');
  assert.equal(session.chapters[1].id, 'pressure');
  assert.equal(session.chapters[2].id, 'settle');
  assert.equal(session.chapters[2].until, 8);
  assert.ok(session.look.sky.startsWith('look/'));
});

test('score and complete pick the chapter the overlay should play', () => {
  const session = defaultSession();
  assert.equal(chapterFromScore(session, 0, false).id, 'explore');
  assert.equal(chapterFromScore(session, 3, false).id, 'pressure');
  assert.equal(chapterFromScore(session, 6, false).id, 'settle');
  assert.equal(chapterFromScore(session, 2, true).id, 'settle');
});

test('look paths stay under public/look, not assets/concepts', () => {
  const paths = lookPaths({
    sky: '/tmp/generated-sky.png',
    ground: '/tmp/generated-ground.png',
    icon: '/tmp/generated-icon.png',
  });
  assert.equal(paths.sky, 'look/sky.png');
  assert.equal(paths.ground, 'look/ground.png');
  assert.equal(paths.icon, 'look/icon.png');
  for (const file of Object.values(paths)) {
    assert.doesNotMatch(file, /concepts/);
  }
});

test('default session has empty model slots for the cast kit', () => {
  const session = defaultSession();
  assert.deepEqual(session.models, {});
});

test('model look paths copy GLB/FBX into public/look, not tripo-character', () => {
  const paths = modelLookPaths({
    player: '/tmp/tripo-character/animated/idle.fbx',
    playerWalk: '/tmp/tripo-character/animated/walk.fbx',
    playerRun: '/tmp/tripo-character/animated/run.fbx',
    enemy: '/tmp/enemy.glb',
    pickup: '/tmp/mark.glb',
  });
  assert.equal(paths.player, 'look/player.fbx');
  assert.equal(paths.playerWalk, 'look/player-walk.fbx');
  assert.equal(paths.playerRun, 'look/player-run.fbx');
  assert.equal(paths.enemy, 'look/enemy.glb');
  assert.equal(paths.pickup, 'look/pickup.glb');
  for (const file of Object.values(paths)) {
    assert.doesNotMatch(file, /tripo-character|concepts/);
  }
});

test('mergeCastModels writes player/enemy/pickup slots into look.json shape', () => {
  const session = mergeCastModels(defaultSession(), {
    player: { file: 'look/player.fbx', walk: 'look/player-walk.fbx', run: 'look/player-run.fbx', height: 1.7 },
    pickup: { file: 'look/pickup.glb' },
  });
  assert.equal(session.models.player.file, 'look/player.fbx');
  assert.equal(session.models.player.walk, 'look/player-walk.fbx');
  assert.equal(session.models.pickup.file, 'look/pickup.glb');
  assert.equal(session.models.enemy, undefined);
});

test('default session is the collect cartridge with density-driven chapters', () => {
  const session = defaultSession();
  assert.equal(session.cartridge, 'collect');
  assert.equal(session.density, 8);
  assert.equal(session.threat, false);
  assert.equal(session.seed, 1);
});

test('jump and unknown prompts map onto the two frozen cartridges', () => {
  assert.equal(normalizeCartridge('jump'), 'jump');
  assert.equal(normalizeCartridge('platformer'), 'jump');
  assert.equal(normalizeCartridge('collect'), 'collect');
  assert.equal(normalizeCartridge('mystery'), 'collect');
});

test('density rescales chapter until values so the last chapter is the pickup count', () => {
  const chapters = scaleChapters(defaultSession().chapters, 6);
  assert.equal(chapters[0].until, 2);
  assert.equal(chapters[1].until, 4);
  assert.equal(chapters[2].until, 6);
});

test('pickup layout is deterministic for a seed and stays inside the arena', () => {
  const a = pickupLayout({ density: 8, seed: 3 });
  const b = pickupLayout({ density: 8, seed: 3 });
  const c = pickupLayout({ density: 8, seed: 9 });
  assert.equal(a.length, 8);
  assert.deepEqual(a, b);
  assert.notDeepEqual(a, c);
  for (const point of a) {
    assert.ok(Math.abs(point.x) <= 9.6);
    assert.ok(Math.abs(point.z) <= 6);
  }
});

test('jump cartridge session gets platforms and does not rewrite collect defaults', () => {
  const jump = withCartridgeSession(defaultSession(), 'jump');
  assert.equal(jump.cartridge, 'jump');
  assert.equal(jump.title, 'Short climb');
  const platforms = platformLayout(jump);
  assert.ok(platforms.length >= 4);
  assert.equal(pickupLayout(jump).length, jump.density);
});

test('share payload marks localhost as a local playtest', () => {
  const local = sharePayload('http://127.0.0.1:5188/', 'Short crossing');
  assert.equal(local.local, true);
  assert.match(local.label, /本地试玩/);
  const remote = sharePayload('https://games.example/ferry', 'Short crossing');
  assert.equal(remote.local, false);
  assert.match(remote.text, /Short crossing/);
});
