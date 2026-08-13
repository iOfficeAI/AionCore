import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  DEFAULT_SEED_TTS_SPEAKER,
  mp3FromTtsStream,
  parseConcatenatedJson,
  resolveSeedTtsApiKey,
  seedTtsRequestBody,
} from './seed_tts_lib.mjs';

test('parses concatenated JSON objects without newlines', () => {
  const a = Buffer.from('ID3fake-a').toString('base64');
  const b = Buffer.from('more').toString('base64');
  const raw = `{"code":0,"message":"","data":"${a}"}{"code":0,"data":"${b}"}{"code":20000000,"message":"ok","data":null}`;
  const objs = parseConcatenatedJson(raw);
  assert.equal(objs.length, 3);
  assert.equal(objs[2].code, 20000000);
  const mp3 = mp3FromTtsStream(raw);
  assert.equal(mp3.toString('utf8'), 'ID3fake-amore');
});

test('raises when a chunk carries a non-success code', () => {
  assert.throws(
    () => mp3FromTtsStream('{"code":55000000,"message":"resource ID is mismatched","data":null}'),
    /55000000/,
  );
});

test('ignores leftover ElevenLabs voice ids so seed-tts-2.0 is not mismatched', () => {
  const body = seedTtsRequestBody({ text: '你好', speaker: 'JBFqnCBsd6RMkjVDRZzb' });
  assert.equal(body.req_params.speaker, DEFAULT_SEED_TTS_SPEAKER);
  const custom = seedTtsRequestBody({ text: '你好', speaker: 'zh_male_m191_uranus_bigtts' });
  assert.equal(custom.req_params.speaker, 'zh_male_m191_uranus_bigtts');
});

test('resolves SEED_TTS_API_KEY before the baked plan key', () => {
  const prevSeed = process.env.SEED_TTS_API_KEY;
  const prevPlan = process.env.AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY;
  process.env.SEED_TTS_API_KEY = 'seed-first';
  process.env.AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY = 'plan-second';
  try {
    assert.equal(resolveSeedTtsApiKey(), 'seed-first');
    assert.equal(resolveSeedTtsApiKey({ ttsApiKey: 'cli' }), 'cli');
  } finally {
    if (prevSeed === undefined) delete process.env.SEED_TTS_API_KEY;
    else process.env.SEED_TTS_API_KEY = prevSeed;
    if (prevPlan === undefined) delete process.env.AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY;
    else process.env.AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY = prevPlan;
  }
});
