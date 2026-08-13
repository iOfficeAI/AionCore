import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./threejs_audio_asset.mjs', import.meta.url));

function run(args, env = {}) {
  return spawnSync(process.execPath, [script, ...args], {
    encoding: 'utf8',
    env: {
      ...process.env,
      ELEVENLABS_API_KEY: '',
      SEED_TTS_API_KEY: '',
      AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY: '',
      ...env,
    },
  });
}

test('probe reports MISSING without a key', () => {
  const result = run(['probe'], { ELEVENLABS_API_KEY: '', SEED_TTS_API_KEY: '' });
  assert.equal(result.status, 0);
  assert.match(result.stdout, /ELEVENLABS_API_KEY=MISSING/);
  assert.match(result.stdout, /SEED_TTS_API_KEY=MISSING/);
});

test('probe reports SET when the env keys are present', () => {
  const result = run(['probe'], { ELEVENLABS_API_KEY: 'sk_test', SEED_TTS_API_KEY: 'ark_test' });
  assert.equal(result.status, 0);
  assert.match(result.stdout, /ELEVENLABS_API_KEY=SET/);
  assert.match(result.stdout, /SEED_TTS_API_KEY=SET/);
});

test('kit refuses to run without a scene', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-audio-kit-'));
  const result = run(['kit', '--genre', 'arcade', '--out', dir, '--dry-run']);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /--scene/);
});

test('kit accepts explore/pressure/settle beats instead of a raw --scene', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-audio-kit-'));
  const result = run([
    'kit',
    '--genre',
    'cozy lantern ferry',
    '--emotion',
    'safety',
    '--verb',
    'carry light across water',
    '--explore',
    'wooden dock at dusk, water lapping',
    '--pressure',
    'crossing chop, lanterns dim',
    '--settle',
    'far pier, warm glass',
    '--spoken',
    'no dialogue',
    '--out',
    dir,
    '--dry-run',
  ]);
  assert.equal(result.status, 0, result.stderr);
  const kit = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'audio', 'kit.json'), 'utf8'));
  assert.match(kit.scene, /wooden dock at dusk/);
  assert.match(kit.scene, /carry light across water/);
  assert.equal(kit.voice.tts, false);
});

test('kit dry-run with spoken lines records intro and settle cues', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-audio-kit-'));
  const result = run([
    'kit',
    '--genre',
    'story climb',
    '--scene',
    '旁白讲述身世',
    '--lines',
    '你还记得上山的路。|不要回头。',
    '--out',
    dir,
    '--dry-run',
  ]);
  assert.equal(result.status, 0, result.stderr);
  const kit = JSON.parse(fs.readFileSync(path.join(dir, 'public', 'audio', 'kit.json'), 'utf8'));
  assert.equal(kit.voice.tts, true);
  assert.equal(kit.voice.lines[0].cue, 'intro');
  assert.equal(kit.voice.lines[1].cue, 'settle');
  assert.equal(kit.voice.lines[0].file, 'audio/voice/line-0.mp3');
});

test('kit --dry-run writes a scene-aware manifest without calling the API', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-audio-kit-'));
  const quiet = run([
    'kit',
    '--genre',
    'cozy lantern ferry',
    '--emotion',
    'safety',
    '--scene',
    'dusk crossing',
    '--out',
    dir,
    '--dry-run',
  ]);
  assert.equal(quiet.status, 0, quiet.stderr);
  const kitPath = path.join(dir, 'public', 'audio', 'kit.json');
  assert.equal(fs.existsSync(kitPath), true);
  const kit = JSON.parse(fs.readFileSync(kitPath, 'utf8'));
  assert.equal(kit.voice.musicVocals, false);
  assert.equal(kit.voice.tts, false);
  assert.ok(kit.music.states.explore);
  assert.match(quiet.stdout, /VOICE=instrumental/);

  const sung = run([
    'kit',
    '--genre',
    'ending credits with lyrics',
    '--scene',
    '旁白送玩家离开',
    '--out',
    dir,
    '--force',
    '--dry-run',
  ]);
  assert.equal(sung.status, 0, sung.stderr);
  const sungKit = JSON.parse(fs.readFileSync(kitPath, 'utf8'));
  assert.equal(sungKit.voice.musicVocals, true);
  assert.equal(sungKit.voice.tts, true);
  assert.match(sung.stdout, /VOICE=vocals\+tts/);
});
