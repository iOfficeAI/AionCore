import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const skillDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

test('probe still prints via a workspace skill symlink', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'aion-3d-skill-'));
  const linked = path.join(root, '.claude', 'skills', 'threejs-3d-generator');
  fs.mkdirSync(path.dirname(linked), { recursive: true });
  fs.symlinkSync(skillDir, linked, 'dir');
  const result = spawnSync(
    process.execPath,
    [path.join(linked, 'scripts', 'threejs_3d_asset.mjs'), 'probe'],
    {
      encoding: 'utf8',
      env: { ...process.env, TRIPO_API_KEY: 'tsk_test' },
    },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /TRIPO_API_KEY=SET/);
});
