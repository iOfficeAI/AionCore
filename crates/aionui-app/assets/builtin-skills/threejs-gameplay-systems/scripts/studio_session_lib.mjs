/** Pure helpers for first-game chapters, look paths, and share copy. No network. */

export const ARENA = { halfWidth: 11, halfDepth: 7 };

function mulberry32(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function normalizeCartridge(value) {
  const text = String(value || '').toLowerCase();
  if (
    text.includes('jump') ||
    text.includes('platform') ||
    text.includes('parkour') ||
    text.includes('跳跃') ||
    text.includes('攀爬') ||
    text.includes('平台')
  ) {
    return 'jump';
  }
  return 'collect';
}

export function clampDensity(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return 8;
  return Math.max(3, Math.min(16, Math.round(n)));
}

export function scaleChapters(chapters, density) {
  const n = clampDensity(density);
  const list = Array.isArray(chapters) ? chapters : [];
  if (!list.length) return list;
  return list.map((chapter, index) => ({
    ...chapter,
    until: Math.max(1, Math.round((n * (index + 1)) / list.length)),
  }));
}

export function pickupLayout(session = {}) {
  const n = clampDensity(session.density ?? session.target ?? 8);
  const rng = mulberry32(Number(session.seed) || 1);
  const maxX = ARENA.halfWidth - 1.5;
  const maxZ = ARENA.halfDepth - 1.2;
  const points = [];
  for (let i = 0; i < n; i += 1) {
    const angle = (i / n) * Math.PI * 2 + rng() * 0.35;
    const radius = 3.2 + rng() * 5.2;
    points.push({
      x: Math.max(-maxX, Math.min(maxX, Math.cos(angle) * radius)),
      z: Math.max(-maxZ, Math.min(maxZ, Math.sin(angle) * radius)),
    });
  }
  return points;
}

export function platformLayout(session = {}) {
  const seed = Number(session.seed) || 1;
  const rng = mulberry32(seed + 17);
  return [
    { x: 0, y: 0, z: 0, w: 7, d: 5, h: 0.55 },
    { x: -7.2, y: 1.15, z: 2.4, w: 4.2, d: 3.2, h: 0.55 },
    { x: 7.4, y: 1.35, z: -1.6, w: 4.4, d: 3.4, h: 0.55 },
    { x: -1.2, y: 2.4, z: -5.1, w: 4.8, d: 3.1, h: 0.55 },
    { x: 3.6, y: 3.5, z: 4.2, w: 3.8 + rng() * 0.4, d: 3.2, h: 0.55 },
  ];
}

export function withCartridgeSession(session, cartridge) {
  const next = normalizeCartridge(cartridge);
  const density = clampDensity(session?.density);
  const base = {
    ...session,
    cartridge: next,
    density,
    seed: Number(session?.seed) || 1,
    threat: Boolean(session?.threat),
    chapters: scaleChapters(session?.chapters || [], density),
  };
  if (next === 'jump') {
    return {
      ...base,
      title: session?.title && session.title !== 'Short crossing' ? session.title : 'Short climb',
      objective:
        session?.objective && session.objective !== 'Collect the marks'
          ? session.objective
          : 'Jump the ledges, collect the marks',
    };
  }
  return base;
}

export function defaultSession() {
  return withCartridgeSession(
    {
      title: 'Short crossing',
      objective: 'Collect the marks',
      chapters: [
      {
        id: 'explore',
        name: 'Approach',
        until: 3,
        status: 'Find the first marks',
        fog: '#1a1814',
        sun: '#fff1bf',
        ground: '#2a2820',
      },
      {
        id: 'pressure',
        name: 'Crossing',
        until: 6,
        status: 'The remaining marks pull harder',
        fog: '#12151a',
        sun: '#c4d4e8',
        ground: '#1e2228',
      },
      {
        id: 'settle',
        name: 'Return',
        until: 8,
        status: 'Bring them home',
        fog: '#1c1610',
        sun: '#ffd9a0',
        ground: '#2c2418',
      },
    ],
    look: {
      sky: 'look/sky.jpg',
      ground: 'look/ground.jpg',
      icon: 'look/icon.png',
    },
      models: {},
    },
    'collect',
  );
}

export function chapterFromScore(session, score, complete) {
  const chapters = session?.chapters || [];
  if (!chapters.length) {
    return { id: 'explore', name: 'Approach', until: 0, status: '' };
  }
  if (complete) return chapters[chapters.length - 1];
  for (const chapter of chapters) {
    if (Number(score) < Number(chapter.until)) return chapter;
  }
  return chapters[chapters.length - 1];
}

function extOf(file, fallback) {
  const match = /\.(png|jpe?g|webp)$/i.exec(String(file || ''));
  if (!match) return fallback;
  const ext = match[0].toLowerCase();
  return ext === '.jpeg' ? '.jpg' : ext;
}

export function lookPaths(input = {}) {
  return {
    sky: `look/sky${extOf(input.sky, '.jpg')}`,
    ground: `look/ground${extOf(input.ground, '.jpg')}`,
    icon: `look/icon${extOf(input.icon, '.png')}`,
  };
}

function ext3d(file, fallback = '.glb') {
  const match = /\.(glb|gltf|fbx)$/i.exec(String(file || ''));
  return match ? match[0].toLowerCase() : fallback;
}

export function modelLookPaths(input = {}) {
  const paths = {};
  if (input.player) paths.player = `look/player${ext3d(input.player)}`;
  if (input.playerWalk) paths.playerWalk = `look/player-walk${ext3d(input.playerWalk)}`;
  if (input.playerRun) paths.playerRun = `look/player-run${ext3d(input.playerRun)}`;
  if (input.enemy) paths.enemy = `look/enemy${ext3d(input.enemy)}`;
  if (input.enemyWalk) paths.enemyWalk = `look/enemy-walk${ext3d(input.enemyWalk)}`;
  if (input.enemyRun) paths.enemyRun = `look/enemy-run${ext3d(input.enemyRun)}`;
  if (input.pickup) paths.pickup = `look/pickup${ext3d(input.pickup)}`;
  return paths;
}

export function mergeCastModels(session, models = {}) {
  return {
    ...session,
    models: {
      ...(session.models || {}),
      ...models,
    },
  };
}

export function sharePayload(url, title = 'Game') {
  const href = String(url || '');
  const local = /localhost|127\.0\.0\.1/.test(href);
  return {
    local,
    label: local ? '本地试玩' : '分享',
    text: local
      ? `${title} — 本地试玩，朋友打不开这台机器上的地址`
      : `Play ${title}`,
    url: href,
  };
}
