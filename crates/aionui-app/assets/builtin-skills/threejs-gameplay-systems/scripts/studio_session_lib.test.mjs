import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  chapterFromScore,
  defaultSession,
  lookPaths,
  sharePayload,
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

test('share payload marks localhost as a local playtest', () => {
  const local = sharePayload('http://127.0.0.1:5188/', 'Short crossing');
  assert.equal(local.local, true);
  assert.match(local.label, /本地试玩/);
  const remote = sharePayload('https://games.example/ferry', 'Short crossing');
  assert.equal(remote.local, false);
  assert.match(remote.text, /Short crossing/);
});
