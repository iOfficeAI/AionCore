#!/usr/bin/env node
/**
 * Generate first-game audio with ElevenLabs.
 * Prefer this Node script. python3 threejs_audio_asset.py is fallback only.
 * Never run bare `python`.
 */

import fs from 'node:fs';
import path from 'node:path';
import {
  buildCompositionPlan,
  buildKitManifest,
  buildMusicPrompt,
  buildSfxJobs,
  requireKitContext,
  resolveKitInput,
  resolveVoicePolicy,
  voiceLineEntries,
} from './audio_kit_lib.mjs';

const BASE_URL = 'https://api.elevenlabs.io/v1';
const DEFAULT_OUTPUT_FORMAT = 'mp3_44100_128';
const DEFAULT_TTS_VOICE_ID = 'JBFqnCBsd6RMkjVDRZzb';

class AudioGeneratorError extends Error {}

function apiKey(args) {
  const key = args.apiKey || process.env.ELEVENLABS_API_KEY;
  if (!key) {
    throw new AudioGeneratorError('Missing API key. Set ELEVENLABS_API_KEY or pass --api-key.');
  }
  return key;
}

function writeFile(filePath, data) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, data);
  console.log(`Audio saved: ${path.resolve(filePath)}`);
}

async function requestBytes(method, apiPath, key, { body, headers, query, timeoutMs = 180_000 } = {}) {
  const url = new URL(`${BASE_URL}${apiPath}`);
  for (const [name, value] of Object.entries(query || {})) {
    if (value != null) url.searchParams.set(name, String(value));
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, {
      method,
      headers: { 'xi-api-key': key, ...headers },
      body,
      signal: controller.signal,
    });
    const bytes = Buffer.from(await response.arrayBuffer());
    if (!response.ok) {
      throw new AudioGeneratorError(`HTTP ${response.status}: ${bytes.toString('utf8')}`);
    }
    return bytes;
  } catch (error) {
    if (error instanceof AudioGeneratorError) throw error;
    if (error.name === 'AbortError') throw new AudioGeneratorError('Network error: timeout');
    throw new AudioGeneratorError(`Network error: ${error.message}`);
  } finally {
    clearTimeout(timer);
  }
}

async function postJsonAudio(args, apiPath, payload, out, outputFormat = DEFAULT_OUTPUT_FORMAT) {
  const data = await requestBytes('POST', apiPath, apiKey(args), {
    body: JSON.stringify(payload),
    headers: { 'Content-Type': 'application/json', Accept: 'audio/mpeg' },
    query: { output_format: args.outputFormat || outputFormat },
  });
  writeFile(out, data);
}

function parseArgs(argv) {
  const args = { _: [], flags: {} };
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    if (token === '--help' || token === '-h') args.help = true;
    else if (token === '--dry-run') args.dryRun = true;
    else if (token === '--force') args.force = true;
    else if (token === '--loop') args.loop = true;
    else if (token === '--validate') args.validate = true;
    else if (token.startsWith('--')) {
      const key = token.slice(2).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      const value = argv[i + 1];
      if (!value || value.startsWith('--')) args[key] = true;
      else {
        args[key] = value;
        i += 1;
      }
    } else args._.push(token);
  }
  args.command = args._.shift();
  return args;
}

function usage() {
  console.log(`Usage:
  node threejs_audio_asset.mjs probe [--validate]
  node threejs_audio_asset.mjs kit --genre <text> --out <game-dir> [--emotion <text>] [--scene <text> | --explore --pressure --settle] [--verb <text>] [--spoken <text>] [--voice auto|on|off] [--lines "a|b"] [--dry-run]
  node threejs_audio_asset.mjs music --out <file> [--genre <text>] [--emotion <text>] [--scene <text>] [--voice auto|on|off]
  node threejs_audio_asset.mjs sfx --prompt <text> --out <file> [--duration 1.2] [--loop]
  node threejs_audio_asset.mjs tts --text <text> --out <file> [--voice-id <id>]`);
}

async function cmdProbe(args) {
  const marker = args.apiKey || process.env.ELEVENLABS_API_KEY ? 'SET' : 'MISSING';
  console.log(`ELEVENLABS_API_KEY=${marker}`);
  if (args.validate && marker === 'SET') {
    const data = await requestBytes('GET', '/user', apiKey(args));
    const user = JSON.parse(data.toString('utf8'));
    console.log(`VALID_USER=${user.email || user.user_id || 'ok'}`);
  }
  return 0;
}

function voiceLabel(policy) {
  if (policy.musicVocals && policy.tts) return 'vocals+tts';
  if (policy.musicVocals) return 'vocals';
  if (policy.tts) return 'tts';
  return 'instrumental';
}

function kitRoot(outDir) {
  return path.join(path.resolve(outDir), 'public');
}

async function generateScore(args, plan, scorePath, input, voice) {
  try {
    await postJsonAudio(args, '/music', plan, scorePath, 'auto');
    return;
  } catch (error) {
    const length = plan.composition_plan.chunks.reduce((sum, chunk) => sum + chunk.duration_ms, 0);
    await postJsonAudio(
      args,
      '/music',
      {
        model_id: 'music_v2',
        prompt: buildMusicPrompt({ ...input, voice }),
        music_length_ms: length,
        force_instrumental: !voice.musicVocals,
      },
      scorePath,
      'auto',
    );
    console.error(`music composition_plan failed, used prompt fallback: ${error.message}`);
  }
}

async function cmdKit(args) {
  if (!args.out) throw new AudioGeneratorError('kit requires --out <game-dir>');
  const input = resolveKitInput({
    genre: args.genre || '',
    emotion: args.emotion || '',
    scene: args.scene || '',
    voice: args.voice || 'auto',
    verb: args.verb || '',
    explore: args.explore || '',
    pressure: args.pressure || '',
    settle: args.settle || '',
    spoken: args.spoken || '',
    sung: args.sung || '',
  });
  const missing = requireKitContext(input);
  if (missing.length) {
    throw new AudioGeneratorError(
      `kit requires --${missing.join(' and --')}. Pass --scene, or --explore --pressure --settle with the heard space.`,
    );
  }
  const voice = resolveVoicePolicy(input);
  const plan = buildCompositionPlan({ ...input, voice });
  const publicRoot = kitRoot(args.out);
  const kitPath = path.join(publicRoot, 'audio', 'kit.json');
  if (fs.existsSync(kitPath) && !args.force && !args.dryRun) {
    throw new AudioGeneratorError(`${kitPath} exists. Pass --force to overwrite.`);
  }

  const scoreRel = 'audio/music/score.mp3';
  const kit = buildKitManifest({ ...input, voice, plan, scorePath: scoreRel });
  kit.sfx = {};
  kit.voice.lines = voice.tts ? voiceLineEntries(args.lines) : [];
  if (voice.tts && kit.voice.lines.length === 0) {
    console.error('TTS_LINES=0 pass --lines "a|b" or the overlay has nothing to speak');
  }

  if (args.dryRun) {
    writeFile(kitPath, Buffer.from(`${JSON.stringify(kit, null, 2)}\n`));
    console.log(`KIT_DRY_RUN path=${kitPath}`);
    console.log(`VOICE=${voiceLabel(voice)} source=${voice.source}`);
    return 0;
  }

  const scorePath = path.join(publicRoot, scoreRel);
  await generateScore(args, plan, scorePath, input, voice);

  for (const job of buildSfxJobs(input)) {
    const out = path.join(publicRoot, job.out);
    await postJsonAudio(
      args,
      '/sound-generation',
      {
        text: job.prompt,
        model_id: 'eleven_text_to_sound_v2',
        prompt_influence: job.loop ? 0.45 : 0.6,
        loop: job.loop,
        duration_seconds: job.duration,
      },
      out,
    );
    kit.sfx[job.id] = { file: job.out, group: job.group, loop: job.loop };
  }

  for (const line of kit.voice.lines) {
    await postJsonAudio(
      args,
      `/text-to-speech/${encodeURIComponent(args.voiceId || DEFAULT_TTS_VOICE_ID)}`,
      { text: line.text, model_id: 'eleven_multilingual_v2' },
      path.join(publicRoot, line.file),
    );
  }

  writeFile(kitPath, Buffer.from(`${JSON.stringify(kit, null, 2)}\n`));
  console.log(`KIT_OK path=${kitPath}`);
  console.log(`VOICE=${voiceLabel(voice)} source=${voice.source}`);
  return 0;
}

async function cmdMusic(args) {
  if (!args.out) throw new AudioGeneratorError('music requires --out <file>');
  const voice = resolveVoicePolicy(args);
  const plan = buildCompositionPlan({ ...args, voice });
  await generateScore(args, plan, path.resolve(args.out), args, voice);
  console.log(`VOICE=${voiceLabel(voice)} source=${voice.source}`);
  return 0;
}

async function cmdSfx(args) {
  if (!args.prompt || !args.out) throw new AudioGeneratorError('sfx requires --prompt and --out');
  const payload = {
    text: args.prompt,
    model_id: args.modelId || 'eleven_text_to_sound_v2',
    prompt_influence: args.promptInfluence == null ? 0.55 : Number(args.promptInfluence),
    loop: Boolean(args.loop),
  };
  if (args.duration != null) payload.duration_seconds = Number(args.duration);
  await postJsonAudio(args, '/sound-generation', payload, path.resolve(args.out));
  return 0;
}

async function cmdTts(args) {
  if (!args.text || !args.out) throw new AudioGeneratorError('tts requires --text and --out');
  await postJsonAudio(
    args,
    `/text-to-speech/${encodeURIComponent(args.voiceId || DEFAULT_TTS_VOICE_ID)}`,
    { text: args.text, model_id: args.modelId || 'eleven_multilingual_v2' },
    path.resolve(args.out),
  );
  return 0;
}

async function main(argv) {
  const args = parseArgs(argv);
  if (args.help || !args.command) {
    usage();
    return args.help ? 0 : 1;
  }
  if (args.command === 'probe') return cmdProbe(args);
  if (args.command === 'kit') return cmdKit(args);
  if (args.command === 'music') return cmdMusic(args);
  if (args.command === 'sfx') return cmdSfx(args);
  if (args.command === 'tts') return cmdTts(args);
  throw new AudioGeneratorError(`unknown command: ${args.command}`);
}

try {
  process.exit(await main(process.argv.slice(2)));
} catch (error) {
  console.error(`threejs_audio_asset.mjs: ${error instanceof Error ? error.message : error}`);
  process.exit(1);
}
