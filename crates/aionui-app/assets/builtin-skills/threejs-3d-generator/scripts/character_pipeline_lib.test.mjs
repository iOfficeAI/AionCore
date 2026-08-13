import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseAnimationList, retargetJobs } from './character_pipeline_lib.mjs';

test('biped idle/walk/run becomes three separate FBX retarget jobs', () => {
  const animations = parseAnimationList('preset:idle,preset:walk,preset:run');
  const jobs = retargetJobs('biped', animations);
  assert.equal(jobs.length, 3);
  assert.deepEqual(
    jobs.map((job) => job.animation || job.animations),
    ['preset:idle', 'preset:walk', 'preset:run'],
  );
  for (const job of jobs) {
    assert.equal(job.type, 'animate_retarget');
    assert.equal(job.out_format, 'fbx');
    assert.equal(job.model_version, undefined);
  }
});

test('quadruped batches clips into one GLB job', () => {
  const jobs = retargetJobs('quadruped', ['preset:quadruped:walk']);
  assert.equal(jobs.length, 1);
  assert.deepEqual(jobs[0].animations, ['preset:quadruped:walk']);
  assert.equal(jobs[0].out_format, 'glb');
  assert.equal(jobs[0].model_version, 'v2.5-20260210');
});
