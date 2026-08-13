export const ARENA = { halfWidth: 11, halfDepth: 7 };

export type Cartridge = 'collect' | 'jump';

export type Chapter = {
  id: string;
  name: string;
  until: number;
  status: string;
  fog?: string;
  sun?: string;
  ground?: string;
  fogNear?: number;
  fogFar?: number;
};

export type Session = {
  title: string;
  objective: string;
  cartridge: Cartridge;
  density: number;
  seed: number;
  threat: boolean;
  chapters: Chapter[];
  look: { sky: string; ground: string; icon: string };
  models?: {
    player?: { file: string; walk?: string; run?: string; height?: number };
    enemy?: { file: string; walk?: string; run?: string; height?: number };
    pickup?: { file: string };
  };
};

export type Point2 = { x: number; z: number };

export type PlatformSpec = {
  x: number;
  y: number;
  z: number;
  w: number;
  d: number;
  h: number;
};

function mulberry32(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function normalizeCartridge(value: string | undefined): Cartridge {
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

export function clampDensity(value: unknown): number {
  const n = Number(value);
  if (!Number.isFinite(n)) return 8;
  return Math.max(3, Math.min(16, Math.round(n)));
}

export function scaleChapters(chapters: Chapter[], density: number): Chapter[] {
  const n = clampDensity(density);
  if (!chapters.length) return chapters;
  return chapters.map((chapter, index) => ({
    ...chapter,
    until: Math.max(1, Math.round((n * (index + 1)) / chapters.length)),
  }));
}

export function pickupLayout(session: Pick<Session, 'density' | 'seed'>): Point2[] {
  const n = clampDensity(session.density);
  const rng = mulberry32(Number(session.seed) || 1);
  const maxX = ARENA.halfWidth - 1.5;
  const maxZ = ARENA.halfDepth - 1.2;
  const points: Point2[] = [];
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

export function platformLayout(session: Pick<Session, 'seed'>): PlatformSpec[] {
  const rng = mulberry32((Number(session.seed) || 1) + 17);
  return [
    { x: 0, y: 0, z: 0, w: 7, d: 5, h: 0.55 },
    { x: -7.2, y: 1.15, z: 2.4, w: 4.2, d: 3.2, h: 0.55 },
    { x: 7.4, y: 1.35, z: -1.6, w: 4.4, d: 3.4, h: 0.55 },
    { x: -1.2, y: 2.4, z: -5.1, w: 4.8, d: 3.1, h: 0.55 },
    { x: 3.6, y: 3.5, z: 4.2, w: 3.8 + rng() * 0.4, d: 3.2, h: 0.55 },
  ];
}

export function withCartridgeSession(session: Partial<Session>, cartridge: string): Session {
  const next = normalizeCartridge(cartridge);
  const density = clampDensity(session.density);
  const base: Session = {
    title: session.title || 'Short crossing',
    objective: session.objective || 'Collect the marks',
    cartridge: next,
    density,
    seed: Number(session.seed) || 1,
    threat: Boolean(session.threat),
    chapters: scaleChapters(session.chapters || defaultChapters(), density),
    look: {
      sky: 'look/sky.jpg',
      ground: 'look/ground.jpg',
      icon: 'look/icon.png',
      ...session.look,
    },
    models: session.models || {},
  };
  if (next === 'jump') {
    return {
      ...base,
      title: session.title && session.title !== 'Short crossing' ? session.title : 'Short climb',
      objective:
        session.objective && session.objective !== 'Collect the marks'
          ? session.objective
          : 'Jump the ledges, collect the marks',
    };
  }
  return base;
}

function defaultChapters(): Chapter[] {
  return [
    {
      id: 'explore',
      name: 'Approach',
      until: 3,
      status: 'Find the first marks',
      fog: '#1a1814',
      sun: '#fff1bf',
      ground: '#2a2820',
      fogNear: 18,
      fogFar: 42,
    },
    {
      id: 'pressure',
      name: 'Crossing',
      until: 6,
      status: 'The remaining marks pull harder',
      fog: '#12151a',
      sun: '#c4d4e8',
      ground: '#1e2228',
      fogNear: 12,
      fogFar: 34,
    },
    {
      id: 'settle',
      name: 'Return',
      until: 8,
      status: 'Bring them home',
      fog: '#1c1610',
      sun: '#ffd9a0',
      ground: '#2c2418',
      fogNear: 16,
      fogFar: 40,
    },
  ];
}

export function defaultSession(): Session {
  return withCartridgeSession(
    {
      title: 'Short crossing',
      objective: 'Collect the marks',
      chapters: defaultChapters(),
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

export function chapterFromScore(session: Session, score: number, complete: boolean): Chapter {
  const chapters = session.chapters;
  if (!chapters.length) {
    return { id: 'explore', name: 'Approach', until: 0, status: session.objective };
  }
  if (complete) return chapters[chapters.length - 1];
  for (const chapter of chapters) {
    if (score < chapter.until) return chapter;
  }
  return chapters[chapters.length - 1];
}

export function sharePayload(url: string, title: string) {
  const local = /localhost|127\.0\.0\.1/.test(url);
  return {
    local,
    label: local ? '本地试玩' : '分享',
    text: local ? `${title} — 本地试玩，朋友打不开这台机器上的地址` : `Play ${title}`,
    url,
  };
}
