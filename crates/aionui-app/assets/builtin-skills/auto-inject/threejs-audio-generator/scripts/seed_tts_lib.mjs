export const SEED_TTS_URL = 'https://openspeech.bytedance.com/api/v3/plan/tts/unidirectional';
export const SEED_TTS_RESOURCE_ID = 'seed-tts-2.0';
export const DEFAULT_SEED_TTS_SPEAKER = 'zh_female_vv_uranus_bigtts';

export function resolveSeedTtsApiKey({ ttsApiKey } = {}) {
  return String(
    ttsApiKey || process.env.SEED_TTS_API_KEY || process.env.AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY || '',
  ).trim();
}

export function resolveSeedTtsSpeaker(speaker) {
  const id = String(speaker || '').trim();
  if (/(?:uranus|saturn)_bigtts/i.test(id) || /^saturn_/i.test(id)) return id;
  return DEFAULT_SEED_TTS_SPEAKER;
}

export function seedTtsRequestBody({ text, speaker }) {
  return {
    user: { uid: 'aion-game-audio' },
    req_params: {
      text,
      speaker: resolveSeedTtsSpeaker(speaker),
      audio_params: { format: 'mp3', sample_rate: 24000, bit_rate: 128000 },
    },
  };
}

export function parseConcatenatedJson(text) {
  const objects = [];
  let i = 0;
  const n = text.length;
  while (i < n) {
    while (i < n && /\s/.test(text[i])) i += 1;
    if (i >= n) break;
    const start = i;
    let depth = 0;
    let inString = false;
    let escaped = false;
    for (; i < n; i += 1) {
      const c = text[i];
      if (inString) {
        if (escaped) escaped = false;
        else if (c === '\\') escaped = true;
        else if (c === '"') inString = false;
        continue;
      }
      if (c === '"') inString = true;
      else if (c === '{') depth += 1;
      else if (c === '}') {
        depth -= 1;
        if (depth === 0) {
          objects.push(JSON.parse(text.slice(start, i + 1)));
          i += 1;
          break;
        }
      }
    }
  }
  return objects;
}

export function mp3FromTtsStream(text) {
  const parts = [];
  for (const obj of parseConcatenatedJson(text)) {
    const code = obj.code;
    if (code != null && code !== 0 && code !== 20000000) {
      throw new Error(`TTS ${code}: ${obj.message || 'error'}`);
    }
    if (typeof obj.data === 'string' && obj.data.length > 0) {
      parts.push(Buffer.from(obj.data, 'base64'));
    }
  }
  if (!parts.length) throw new Error('TTS returned no audio');
  return Buffer.concat(parts);
}
