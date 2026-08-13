import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  buildCompositionPlan,
  buildKitManifest,
  buildMusicPrompt,
  buildSceneFromBeats,
  buildSfxJobs,
  eventsFromDiagnostics,
  musicStateFromProgress,
  requireKitContext,
  resolveKitInput,
  resolveVoicePolicy,
} from './audio_kit_lib.mjs';

test('default scene is instrumental only', () => {
  const policy = resolveVoicePolicy({
    genre: 'cozy lantern ferry',
    emotion: 'safety, attachment',
    scene: 'dusk crossing, collect lanterns',
  });
  assert.equal(policy.musicVocals, false);
  assert.equal(policy.tts, false);
});

test('spoken scenes request TTS; sung scenes request music vocals', () => {
  const narrated = resolveVoicePolicy({ scene: '旁白讲述身世后进入第一章' });
  assert.equal(narrated.tts, true);
  assert.equal(narrated.musicVocals, false);
  assert.equal(resolveVoicePolicy({ genre: 'rhythm game with lyrics' }).musicVocals, true);
  assert.equal(resolveVoicePolicy({ scene: 'announcer calls each checkpoint' }).tts, true);
});

test('no dialogue does not request TTS', () => {
  const policy = resolveVoicePolicy({
    scene: 'wooden dock at dusk, water lapping, no dialogue',
  });
  assert.equal(policy.tts, false);
  assert.equal(policy.musicVocals, false);
});

test('explicit voice flags override inference', () => {
  assert.equal(resolveVoicePolicy({ scene: '旁白', voice: 'off' }).musicVocals, false);
  assert.equal(resolveVoicePolicy({ genre: 'cozy ferry', voice: 'on' }).musicVocals, true);
});

test('instrumental plan matches ElevenLabs music_v2 chunk contract', () => {
  const quiet = buildCompositionPlan({
    genre: 'cozy lantern ferry',
    emotion: 'safety, attachment',
    scene: 'wooden dock at dusk, water lapping, no dialogue',
    voice: resolveVoicePolicy({ genre: 'cozy lantern ferry' }),
  });
  assert.equal(quiet.model_id, 'music_v2');
  assert.equal(quiet.composition_plan.chunks.length, 3);
  assert.ok(quiet.composition_plan.chunks[0].positive_styles.length >= 6);
  for (const style of quiet.composition_plan.chunks[0].positive_styles) {
    assert.doesNotMatch(style, /[\u4e00-\u9fff]/);
  }
  for (const chunk of quiet.composition_plan.chunks) {
    assert.match(chunk.text, /^\[(explore|pressure|settle)\]/i);
    assert.match(chunk.text, /\{instrumental\}/i);
    assert.ok(chunk.negative_styles.includes('vocals'));
    assert.ok(chunk.duration_ms >= 3000);
  }
  assert.match(quiet.composition_plan.chunks[0].text, /wooden dock|dusk|water/i);

  const sung = buildCompositionPlan({
    genre: 'ending credits',
    emotion: 'aftertaste',
    scene: 'credits, soft choir',
    voice: resolveVoicePolicy({ voice: 'on' }),
  });
  for (const chunk of sung.composition_plan.chunks) {
    assert.doesNotMatch(chunk.text, /\{instrumental\}/i);
    assert.ok(!chunk.negative_styles.includes('vocals'));
    assert.ok(chunk.positive_styles.some((style) => /vocal|choir|sing/i.test(style)));
  }
});

test('Chinese scene still yields English styles', () => {
  const plan = buildCompositionPlan({
    genre: '温馨灯笼渡口',
    emotion: '安全感',
    scene: '黄昏木码头，水声，无对白',
    voice: resolveVoicePolicy({ scene: '黄昏木码头，水声，无对白' }),
  });
  const styles = plan.composition_plan.chunks[0].positive_styles;
  assert.ok(styles.length >= 6);
  for (const style of styles) {
    assert.doesNotMatch(style, /[\u4e00-\u9fff]/);
  }
  assert.ok(styles.some((style) => /cozy|warm|gentle|dusk|water|wood/i.test(style)));
});

test('prompt fallback is instrumental-safe and English', () => {
  const prompt = buildMusicPrompt({
    genre: '温馨灯笼渡口',
    emotion: '安全感',
    scene: '黄昏木码头，水声，无对白',
    voice: resolveVoicePolicy({ scene: '黄昏木码头，无对白' }),
  });
  assert.doesNotMatch(prompt, /[\u4e00-\u9fff]/);
  assert.match(prompt, /instrumental/i);
  assert.match(prompt, /explore/i);
  assert.match(prompt, /no vocals/i);
});

test('sfx durations stay inside the official 0.5-30s window and include the scene', () => {
  const jobs = buildSfxJobs({
    genre: 'cozy lantern ferry',
    scene: 'wooden dock, water, lantern glass',
  });
  for (const job of jobs) {
    assert.ok(job.duration >= 0.5);
    assert.ok(job.duration <= 30);
    assert.match(job.prompt, /no music/i);
  }
  assert.match(jobs.find((job) => job.id === 'pickup').prompt, /wooden dock|lantern/i);
});

test('kit manifest stores one score and three loop regions', () => {
  const plan = buildCompositionPlan({
    genre: 'arcade dash',
    emotion: 'mastery',
    voice: resolveVoicePolicy({ genre: 'arcade dash' }),
  });
  const kit = buildKitManifest({
    genre: 'arcade dash',
    voice: resolveVoicePolicy({ genre: 'arcade dash' }),
    plan,
    scorePath: 'audio/music/score.mp3',
  });
  assert.equal(kit.music.file, 'audio/music/score.mp3');
  assert.deepEqual(Object.keys(kit.music.states), ['explore', 'pressure', 'settle']);
  assert.equal(kit.music.states.explore.start, 0);
  assert.ok(kit.music.states.pressure.start > 0);
  assert.ok(kit.music.states.settle.start > kit.music.states.pressure.start);
  assert.equal(kit.voice.musicVocals, false);
});

test('sfx jobs stay event-sized and never ask for music', () => {
  const jobs = buildSfxJobs({ genre: 'cozy lantern ferry' });
  assert.deepEqual(
    jobs.map((job) => job.id),
    ['pickup', 'dash', 'hit', 'fail', 'confirm', 'pause', 'win', 'ambience'],
  );
  assert.equal(jobs.find((job) => job.id === 'ambience').loop, true);
});

test('progress maps to explore, pressure, settle', () => {
  assert.equal(musicStateFromProgress({ score: 0, target: 8, complete: false }), 'explore');
  assert.equal(musicStateFromProgress({ score: 4, target: 8, complete: false }), 'pressure');
  assert.equal(musicStateFromProgress({ score: 8, target: 8, complete: true }), 'settle');
});

test('chapter and failed override score-based music state', () => {
  assert.equal(musicStateFromProgress({ score: 0, target: 8, chapter: 'pressure' }), 'pressure');
  assert.equal(musicStateFromProgress({ score: 7, target: 8, chapter: 'explore' }), 'explore');
  assert.equal(musicStateFromProgress({ score: 1, target: 8, chapter: 3 }), 'settle');
  assert.equal(musicStateFromProgress({ score: 0, target: 8, failed: true }), 'pressure');
  assert.equal(musicStateFromProgress({ score: 8, target: 8, complete: true, chapter: 'explore' }), 'settle');
});

test('beat flags compose a scene the kit can send to ElevenLabs', () => {
  const scene = buildSceneFromBeats({
    verb: 'carry light across water',
    explore: 'wooden dock at dusk, water lapping',
    pressure: 'crossing chop, lanterns dim',
    settle: 'far pier, warm glass',
    spoken: 'no dialogue',
  });
  assert.match(scene, /carry light across water/);
  assert.match(scene, /explore: wooden dock/);
  assert.match(scene, /pressure: crossing chop/);
  assert.match(scene, /settle: far pier/);
  assert.match(scene, /no dialogue/);
  const input = resolveKitInput({
    genre: 'cozy lantern ferry',
    explore: 'wooden dock at dusk',
    pressure: 'crossing chop',
    settle: 'far pier',
  });
  assert.equal(requireKitContext(input).length, 0);
  assert.match(input.scene, /wooden dock/);
});

test('diagnostics rising edges become playable audio events', () => {
  const start = { score: 0, complete: false, player: { speed: 2 } };
  assert.deepEqual(
    eventsFromDiagnostics(start, { score: 1, complete: false, player: { speed: 2 } }).map((e) => e.id),
    ['pickup'],
  );
  assert.deepEqual(
    eventsFromDiagnostics(start, { score: 0, complete: true, player: { speed: 2 } }).map((e) => e.id),
    ['win', 'voice:settle'],
  );
  assert.ok(
    eventsFromDiagnostics(start, { score: 0, complete: false, player: { speed: 9.2 } }).some((e) => e.id === 'dash'),
  );
  assert.ok(
    eventsFromDiagnostics(start, { score: 0, complete: false, failed: true, player: { speed: 2 } }).some(
      (e) => e.id === 'fail',
    ),
  );
  assert.ok(
    eventsFromDiagnostics(start, { score: 0, complete: false, hits: 1, player: { speed: 2 } }).some((e) => e.id === 'hit'),
  );
  assert.ok(
    eventsFromDiagnostics(start, { score: 0, complete: false, paused: true, player: { speed: 2 } }).some(
      (e) => e.id === 'pause',
    ),
  );
  assert.deepEqual(eventsFromDiagnostics(start, start), []);
});
