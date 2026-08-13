#!/usr/bin/env node
/** Audit a Three.js game director final report. Node twin of audit_reference_report.py. */

import fs from 'node:fs';
import process from 'node:process';

const BASE_REQUIRED = [
  'skill-loading ledger',
  'reference ledger',
  'phase ledger',
  'gameplay systems',
  'aaa graphics',
  'ui',
  'debug/profile',
  'qa/release',
];

const DESIGN_REQUIRED = ['game design brief', 'core loop', 'level/encounter plan'];

// aion: experience gate — keep as one block for vendor rebase
const AION_EXPERIENCE_REQUIRED = ['experience intent', 'emotion beat', 'chapter ledger', 'share'];
const AION_PLAY_MARKERS = ['play', 'launch'];
const AION_MUSIC_MARKERS = ['music', 'audio state'];

const PHYSICS_MARKERS = ['physics engine', 'timestep', 'collider'];

const PREMIUM_SCORECARD = [
  'art direction',
  'hero/player',
  'obstacles/enemies',
  'rewards/interactables',
  'world/environment',
  'materials/textures',
  'lighting/render',
  'vfx/motion',
  'ui/hud',
  'performance evidence',
  'measured evidence',
  'fresh-eyes review',
  'average',
  'automatic failures',
];

const PREMIUM_ASSET_SOURCING = [
  'external asset sourcing',
  'credential probe output',
  'tripo_api_key=',
  'gemini_api_key=',
  '3d generator',
  'image generator',
  'chosen sources',
  'hero/player',
  'world/sky/background',
  'materials/textures/decals',
];

const PREMIUM_TECHNICAL_ART = ['technical art', 'render budget', 'vfx readability'];
const PREMIUM_VISUAL_HARNESS = ['visual test harness'];
const PREMIUM_AUDIO = ['audio', 'audio generator', 'elevenlabs_api_key='];

const EXTERNAL_OUTPUT_PATTERNS = [
  /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/,
  /\b[\w./-]*assets\/(models|concepts|textures|ui|images|audio)\/[\w./-]+\.(glb|gltf|fbx|png|jpg|jpeg|webp|mp3|wav|ogg|m4a)\b/,
  /\b[\w./-]+\.(glb|gltf|fbx)\b/,
];

const AUDIO_OUTPUT_PATTERNS = [/\b[\w./-]*assets\/audio\/[\w./-]+\.(mp3|wav|ogg|m4a)\b/];

const NON_CREDENTIAL_BLOCKER_MARKERS = [
  'api error',
  'network error',
  'quota',
  'offline-only',
  'offline only',
  'user requested no external',
  'no external ai',
  'no external assets',
];

const VERIFICATION_MARKERS = [
  'build',
  'console',
  'page error',
  'desktop',
  'mobile',
  'screenshot',
  'canvas',
  'pixel',
];

function normalize(text) {
  text = text.toLowerCase();
  const replacements = [
    ['skill loading ledger', 'skill-loading ledger'],
    ['skill loaded ledger', 'skill-loading ledger'],
    ['reference loading ledger', 'reference ledger'],
    ['asset sourcing ledger', 'external asset sourcing'],
    ['external asset ledger', 'external asset sourcing'],
    ['gameplay brief', 'game design brief'],
    ['design brief', 'game design brief'],
    ['playable loop', 'core loop'],
    ['level plan', 'level/encounter plan'],
    ['encounter plan', 'level/encounter plan'],
    ['level and encounter plan', 'level/encounter plan'],
    ['technical-art', 'technical art'],
    ['technical art budget', 'technical art render budget'],
    ['render-budget', 'render budget'],
    ['visual harness', 'visual test harness'],
    ['screenshot baseline', 'visual test harness'],
    ['threejs-3d-generator', '3d generator'],
    ['threejs-image-generator', 'image generator'],
    ['threejs-audio-generator', 'audio generator'],
    ['tripo 3d assets', '3d generator'],
    ['tripo 3d generation', '3d generator'],
    ['tripo 3d', '3d generator'],
    ['tripo loaded', '3d generator loaded'],
    ['nano banana pro', 'image generator'],
    ['nano banana', 'image generator'],
    ['nanobanana', 'image generator'],
    ['nano-banana', 'image generator'],
    ['phase-execution ledger', 'phase ledger'],
    ['phase execution ledger', 'phase ledger'],
    ['debug and profile', 'debug/profile'],
    ['debug profile', 'debug/profile'],
    ['qa and release', 'qa/release'],
    ['qa release', 'qa/release'],
    ['page errors', 'page error'],
    ['fresh eyes review', 'fresh-eyes review'],
    ['fresh-eyes scorecard review', 'fresh-eyes review'],
    ['independent reviewer scores', 'fresh-eyes review'],
    ['adversarial self-review', 'fresh-eyes review'],
    ['measured visual evidence', 'measured evidence'],
    ['inspector metrics', 'measured evidence'],
    // aion: experience gate aliases
    ['体验意图', 'experience intent'],
    ['情绪节拍', 'emotion beat'],
    ['content ledger', 'chapter ledger'],
    ['章节', 'chapter ledger'],
    ['分享', 'share'],
    ['实际启动', 'play'],
    ['配乐', 'music'],
  ];
  for (const [from, to] of replacements) {
    text = text.split(from).join(to);
  }
  return text.replace(/\s+/g, ' ');
}

function markerPattern(marker) {
  const prefix = /^\w/.test(marker) ? '\\b' : '';
  const suffix = /\w$/.test(marker) ? '\\b' : '';
  return new RegExp(prefix + marker.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + suffix);
}

function missingMarkers(text, markers) {
  return markers.filter((marker) => !markerPattern(marker).test(text));
}

function missingAnyGroup(text, markers, label) {
  return markers.some((marker) => markerPattern(marker).test(text)) ? [] : [label];
}

function hasExternalOutputEvidence(text) {
  return EXTERNAL_OUTPUT_PATTERNS.some((pattern) => pattern.test(text));
}

function hasAudioOutputEvidence(text) {
  return AUDIO_OUTPUT_PATTERNS.some((pattern) => pattern.test(text));
}

function hasExternalBlocker(text) {
  const bothMissing = text.includes('tripo_api_key=missing') && text.includes('gemini_api_key=missing');
  const nonCredential = NON_CREDENTIAL_BLOCKER_MARKERS.some((marker) => text.includes(marker));
  return bothMissing || nonCredential;
}

function hasAudioBlocker(text) {
  return (
    text.includes('elevenlabs_api_key=missing') ||
    NON_CREDENTIAL_BLOCKER_MARKERS.some((marker) => text.includes(marker))
  );
}

function parseArgs(argv) {
  const flags = { premium: false, physics: false, audio: false, noDesign: false, report: null };
  for (const arg of argv) {
    if (arg === '--premium') flags.premium = true;
    else if (arg === '--physics') flags.physics = true;
    else if (arg === '--audio') flags.audio = true;
    else if (arg === '--no-design') flags.noDesign = true;
    else if (arg === '--help' || arg === '-h') {
      console.log('Check that a Three.js director final report includes required evidence.');
      console.log('Usage: node audit_reference_report.mjs [--premium] [--physics] [--audio] [--no-design] <report>');
      process.exit(0);
    } else if (arg.startsWith('-')) {
      throw new Error(`unknown flag: ${arg}`);
    } else if (flags.report) {
      throw new Error('Usage: node audit_reference_report.mjs [--premium] [--physics] [--audio] [--no-design] <report>');
    } else {
      flags.report = arg;
    }
  }
  if (!flags.report) {
    throw new Error('Usage: node audit_reference_report.mjs [--premium] [--physics] [--audio] [--no-design] <report>');
  }
  return flags;
}

function main(argv) {
  const args = parseArgs(argv);
  if (!fs.existsSync(args.report)) {
    console.error(`Missing report file: ${args.report}`);
    return 1;
  }

  const text = normalize(fs.readFileSync(args.report, 'utf8'));
  const missing = missingMarkers(text, BASE_REQUIRED);
  if (!args.noDesign) {
    missing.push(...missingMarkers(text, DESIGN_REQUIRED));
    missing.push(...missingMarkers(text, AION_EXPERIENCE_REQUIRED));
    missing.push(...missingAnyGroup(text, AION_PLAY_MARKERS, 'play'));
    missing.push(...missingAnyGroup(text, AION_MUSIC_MARKERS, 'music'));
  }

  if (args.premium) {
    missing.push(...missingMarkers(text, PREMIUM_SCORECARD));
    missing.push(...missingMarkers(text, PREMIUM_ASSET_SOURCING));
    missing.push(...missingMarkers(text, PREMIUM_TECHNICAL_ART));
    missing.push(...missingMarkers(text, PREMIUM_VISUAL_HARNESS));
    missing.push(...missingMarkers(text, VERIFICATION_MARKERS));
    if (!hasExternalOutputEvidence(text) && !hasExternalBlocker(text)) {
      missing.push('real external asset evidence or blocker');
    }
    if (
      text.includes('not-needed') &&
      text.includes('procedural') &&
      !hasExternalOutputEvidence(text) &&
      !hasExternalBlocker(text)
    ) {
      missing.push('procedural/not-needed requires external output evidence or blocker');
    }
  }

  if (args.physics) {
    missing.push(...missingMarkers(text, PHYSICS_MARKERS));
  }

  if (args.audio) {
    missing.push(...missingMarkers(text, PREMIUM_AUDIO));
    if (!hasAudioOutputEvidence(text) && !hasAudioBlocker(text)) {
      missing.push('real audio asset evidence or blocker');
    }
  }

  if (missing.length > 0) {
    console.log('Director report audit failed. Missing required markers:');
    for (const marker of missing) {
      console.log(`- ${marker}`);
    }
    return 1;
  }

  console.log('Director report audit passed.');
  return 0;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
