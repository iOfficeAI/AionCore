/** Pure helpers for Tripo character retarget jobs. No network. */

import fs from 'node:fs';
import path from 'node:path';

export const RETARGET_BATCH_LIMIT = 5;
export const CREATURE_RIG_VERSION = 'v2.5-20260210';

export function parseAnimationList(raw) {
  return String(raw ?? 'preset:idle,preset:walk,preset:run')
    .split(',')
    .map((item) => item.trim())
    .filter(Boolean);
}

export function retargetJobs(rigType, animations, options = {}) {
  const version = options.rigModelVersion;
  const legacy = rigType === 'biped' || String(version || '').startsWith('v1.0');
  if (legacy) {
    return animations.map((animation) => ({
      type: 'animate_retarget',
      animation,
      out_format: 'fbx',
    }));
  }
  const jobs = [];
  for (let i = 0; i < animations.length; i += RETARGET_BATCH_LIMIT) {
    jobs.push({
      type: 'animate_retarget',
      animations: animations.slice(i, i + RETARGET_BATCH_LIMIT),
      out_format: 'glb',
      model_version: version || CREATURE_RIG_VERSION,
    });
  }
  return jobs;
}

export function clipNameFromPreset(preset) {
  const parts = String(preset).split(':');
  return parts.at(-1) || 'clip';
}

export function findDownloadedModel(dir) {
  const files = [];
  const walk = (current) => {
    if (!fs.existsSync(current)) return;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) walk(full);
      else files.push(full);
    }
  };
  walk(dir);
  return files.find((file) => /\.fbx$/i.test(file)) || files.find((file) => /\.glb$/i.test(file)) || null;
}
