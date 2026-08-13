/** Pure helpers for the first-game ElevenLabs audio kit. No network. */

const MUSIC_VOCAL_RE =
  /歌词|歌唱|演唱|合唱|主题曲|rap|lyric|lyrics|singing|sung|vocal|vocals|choir|theme song/i;
const TTS_RE =
  /旁白|对白|台词|解说|口述|narrat|announcer|dialogue|dialog|voice.?over|spoken|tutorial (line|voice)/i;
const FORCE_OFF_RE = /器乐|纯音乐|无人声|instrumental only|no vocals?/i;
const NO_TTS_RE = /无对白|无旁白|无人说话|no dialogue|no dialog|no spoken|no voice-?over/i;

const CHUNK_MS = {
  explore: 28_000,
  pressure: 28_000,
  settle: 20_000,
};

const BASE_STYLES = ['game soundtrack', 'great production quality', 'clear mix', 'loop-friendly'];

const STYLE_GLOSSARY = [
  [/治愈|温馨|安宁|灯笼|渡口|ferry|cozy|lantern/, ['cozy', 'warm', 'gentle']],
  [/恐怖|黑暗|惊悚|horror|dark|tense/, ['dark', 'tense', 'sparse']],
  [/竞速|赛车|racing|dash|arcade/, ['arcade', 'driving', 'rhythmic']],
  [/解谜|puzzle|curious/, ['curious', 'minimal', 'piano']],
  [/动作|action|combat/, ['punchy', 'percussive', 'brass']],
  [/叙事|旁白|narrat|story/, ['narrative', 'restrained']],
  [/黄昏|夜晚|dusk|night/, ['dusk', 'nocturnal']],
  [/水|海|雨|water|surf|rain/, ['water', 'ambient']],
  [/木|码头|wood|dock/, ['acoustic', 'wood']],
  [/安全|依恋|safety|attachment/, ['safe', 'soft']],
  [/掌控|刺激|mastery|thrill/, ['confident', 'bright']],
];

const SFX_JOBS = [
  {
    id: 'pickup',
    group: 'sfx',
    duration: 0.9,
    loop: false,
    prompt:
      'short collect pickup for {genre}, {scene}, bright material tap, clear transient, 0.3s sparkle tail, no music, no voice',
  },
  {
    id: 'dash',
    group: 'sfx',
    duration: 0.7,
    loop: false,
    prompt:
      'short dash whoosh for {genre}, {scene}, air burst, tight transient, 0.2s tail, no music, no voice',
  },
  {
    id: 'hit',
    group: 'sfx',
    duration: 0.8,
    loop: false,
    prompt:
      'short impact hit for {genre}, {scene}, body contact, low thump, 0.25s tail, no music, no voice',
  },
  {
    id: 'fail',
    group: 'sfx',
    duration: 1.1,
    loop: false,
    prompt:
      'short fail sting for {genre}, {scene}, downward muted hit, readable under mix, no music, no voice',
  },
  {
    id: 'confirm',
    group: 'ui',
    duration: 0.5,
    loop: false,
    prompt: 'tiny menu confirm click, soft latch, short warm tail, no music, no voice',
  },
  {
    id: 'pause',
    group: 'ui',
    duration: 0.5,
    loop: false,
    prompt: 'tiny pause open tick, muted wood, no music, no voice',
  },
  {
    id: 'win',
    group: 'ui',
    duration: 1.4,
    loop: false,
    prompt: 'short win chime for {genre}, two-note resolve, no lyrics, no music bed, no voice',
  },
  {
    id: 'ambience',
    group: 'ambience',
    duration: 12,
    loop: true,
    prompt:
      'seamless room-tone ambience for {genre}, {scene}, space only, no melody, no music, no voice',
  },
];

const SECTION_ENERGY = {
  explore: ['establishing', 'mid-low energy', 'readable'],
  pressure: ['rising tension', 'higher pulse', 'same score family'],
  settle: ['resolving', 'aftertaste', 'lower density'],
};

export function sceneText(input = {}) {
  return [input.genre, input.emotion, input.scene, input.brief]
    .filter(Boolean)
    .join(' ')
    .trim();
}

export function resolveVoicePolicy(input = {}) {
  const voice = String(input.voice || 'auto').toLowerCase();
  if (voice === 'on' || voice === 'true' || voice === 'required') {
    return { musicVocals: true, tts: true, source: 'flag' };
  }
  if (voice === 'off' || voice === 'false' || voice === 'none') {
    return { musicVocals: false, tts: false, source: 'flag' };
  }

  const text = sceneText(input);
  if (FORCE_OFF_RE.test(text)) {
    return { musicVocals: false, tts: false, source: 'scene' };
  }
  return {
    musicVocals: MUSIC_VOCAL_RE.test(text),
    tts: TTS_RE.test(text) && !NO_TTS_RE.test(text),
    source: 'scene',
  };
}

function isEnglishStyle(token) {
  return /^[a-z][a-z0-9 \-']{0,40}$/i.test(token) && !/[\u4e00-\u9fff]/.test(token);
}

function uniqueStyles(list) {
  const seen = new Set();
  const out = [];
  for (const raw of list) {
    const token = String(raw || '')
      .trim()
      .toLowerCase();
    if (!token || !isEnglishStyle(token) || seen.has(token)) continue;
    seen.add(token);
    out.push(token);
  }
  return out;
}

export function buildEnglishStyles(input = {}) {
  const text = sceneText(input);
  const mapped = [];
  for (const [pattern, styles] of STYLE_GLOSSARY) {
    if (pattern.test(text)) mapped.push(...styles);
  }
  const ascii = [];
  for (const part of text.split(/[,/|，、]+/)) {
    const token = part.trim();
    if (isEnglishStyle(token) && token.split(/\s+/).length <= 4) ascii.push(token);
  }
  return uniqueStyles([...ascii, ...mapped, ...BASE_STYLES]);
}

function englishCues(input = {}) {
  const styles = buildEnglishStyles(input);
  const sceneAscii = String(input.scene || '')
    .split(/[,/|，、]+/)
    .map((part) => part.trim())
    .filter((part) => isEnglishStyle(part));
  return uniqueStyles([...sceneAscii, ...styles.slice(0, 4)]).slice(0, 6);
}

function chunkText(name, input, voice) {
  const cues = englishCues(input);
  const cue = cues.length ? `{${cues.join(', ')}}` : '';
  if (voice.musicVocals) {
    return [`[${name}]`, '{soft vocals}', cue].filter(Boolean).join(' ');
  }
  return [`[${name}]`, '{instrumental}', cue].filter(Boolean).join(' ');
}

export function buildCompositionPlan(input = {}) {
  const voice = input.voice || resolveVoicePolicy(input);
  const base = buildEnglishStyles(input);
  if (base.length < 6) {
    base.push('cinematic', 'mid tempo');
  }
  const chunks = ['explore', 'pressure', 'settle'].map((name, index) => {
    const local = uniqueStyles([
      ...base,
      ...SECTION_ENERGY[name],
      voice.musicVocals ? 'vocals' : 'instrumental',
    ]);
    if (index === 0 && local.length < 6) {
      local.push('game score', 'warm tone');
    }
    return {
      text: chunkText(name, input, voice),
      duration_ms: CHUNK_MS[name],
      positive_styles: local.slice(0, 12),
      negative_styles: voice.musicVocals
        ? ['radio edit', 'spoken advertisement']
        : ['vocals', 'lyrics', 'singing', 'choir'],
      context_adherence: 'high',
    };
  });

  return {
    model_id: 'music_v2',
    composition_plan: { chunks },
  };
}

export function buildMusicPrompt(input = {}) {
  const voice = input.voice || resolveVoicePolicy(input);
  const styles = buildEnglishStyles(input).join(', ') || 'game soundtrack';
  const cues = englishCues(input).join(', ');
  if (voice.musicVocals) {
    return `Game soundtrack with scene-fit vocals. Styles: ${styles}. Scene: ${cues}. Structure: explore, then pressure, then settle. No radio DJ, no ads.`;
  }
  return `Instrumental only game soundtrack, no vocals, no lyrics, no choir. Styles: ${styles}. Scene: ${cues}. Structure: explore, then pressure, then settle. Loop-friendly.`;
}

export function loopRegionsFromPlan(plan) {
  const states = {};
  let cursor = 0;
  for (const chunk of plan.composition_plan.chunks) {
    const name = /\[(\w+)\]/.exec(chunk.text)?.[1] || `part${cursor}`;
    const duration = chunk.duration_ms / 1000;
    states[name] = { start: cursor, duration };
    cursor += duration;
  }
  return states;
}

export function buildKitManifest(input = {}) {
  const plan = input.plan || buildCompositionPlan(input);
  const voice = input.voice || resolveVoicePolicy(input);
  return {
    version: 1,
    genre: input.genre || 'arcade game',
    emotion: input.emotion || '',
    scene: input.scene || '',
    voice: {
      musicVocals: Boolean(voice.musicVocals),
      tts: Boolean(voice.tts),
      source: voice.source || 'scene',
    },
    music: {
      file: input.scorePath || 'audio/music/score.mp3',
      states: loopRegionsFromPlan(plan),
    },
    groups: ['master', 'music', 'ambience', 'sfx', 'ui', 'voice'],
  };
}

export function buildSfxJobs(input = {}) {
  const genre = input.genre || 'arcade game';
  const scene = input.scene || genre;
  return SFX_JOBS.map((job) => ({
    ...job,
    prompt: job.prompt.replaceAll('{genre}', genre).replaceAll('{scene}', scene),
    out: job.id === 'ambience' ? `audio/ambience/space.mp3` : `audio/${job.group}/${job.id}.mp3`,
  }));
}

export function musicStateFromProgress(progress = {}) {
  if (progress.complete) return 'settle';
  if (progress.failed) return 'pressure';
  const chapter = String(progress.chapter ?? progress.musicState ?? '')
    .trim()
    .toLowerCase();
  if (chapter === 'explore' || chapter === 'pressure' || chapter === 'settle') return chapter;
  if (chapter === '1' || chapter === 'intro') return 'explore';
  if (chapter === '2' || chapter === 'mid') return 'pressure';
  if (chapter === '3' || chapter === 'end') return 'settle';
  const target = Number(progress.target) || 0;
  const score = Number(progress.score) || 0;
  if (target > 0 && score >= target * 0.5) return 'pressure';
  return 'explore';
}

export function buildSceneFromBeats(input = {}) {
  const parts = [];
  if (input.verb) parts.push(String(input.verb).trim());
  if (input.explore) parts.push(`explore: ${String(input.explore).trim()}`);
  if (input.pressure) parts.push(`pressure: ${String(input.pressure).trim()}`);
  if (input.settle) parts.push(`settle: ${String(input.settle).trim()}`);
  if (input.spoken) parts.push(String(input.spoken).trim());
  if (input.sung) parts.push(String(input.sung).trim());
  return parts.filter(Boolean).join(', ');
}

export function resolveKitInput(input = {}) {
  const scene = String(input.scene || '').trim() || buildSceneFromBeats(input);
  return { ...input, scene };
}

export function eventsFromDiagnostics(prev, next) {
  const events = [];
  if (!next) return events;
  const before = prev || {};
  if ((Number(next.score) || 0) > (Number(before.score) || 0)) events.push({ id: 'pickup' });
  if (next.complete && !before.complete) {
    events.push({ id: 'win' });
    events.push({ id: 'voice:settle' });
  }
  if (next.failed && !before.failed) events.push({ id: 'fail' });
  const hits = Number(next.hits) || (next.hit ? 1 : 0);
  const prevHits = Number(before.hits) || (before.hit ? 1 : 0);
  if (hits > prevHits) events.push({ id: 'hit' });
  if (next.paused && !before.paused) events.push({ id: 'pause' });
  if (next.dashing && !before.dashing) events.push({ id: 'dash' });
  const speed = Number(next.player?.speed) || 0;
  const prevSpeed = Number(before.player?.speed) || 0;
  if (!next.dashing && speed >= 7.2 && prevSpeed < 7.2) events.push({ id: 'dash' });
  return events;
}

export function voiceLineEntries(lines) {
  const texts = parseVoiceLines(lines);
  return texts.map((text, index) => ({
    id: `line-${index}`,
    text,
    file: `audio/voice/line-${index}.mp3`,
    cue: index === 0 ? 'intro' : index === texts.length - 1 ? 'settle' : `line-${index}`,
  }));
}

export function parseVoiceLines(raw) {
  if (!raw) return [];
  if (Array.isArray(raw)) return raw.map((line) => String(line).trim()).filter(Boolean);
  return String(raw)
    .split('|')
    .map((line) => line.trim())
    .filter(Boolean);
}

export function requireKitContext(input = {}) {
  const resolved = resolveKitInput(input);
  const missing = [];
  if (!String(resolved.genre || '').trim()) missing.push('genre');
  if (!String(resolved.scene || '').trim()) missing.push('scene');
  return missing;
}
