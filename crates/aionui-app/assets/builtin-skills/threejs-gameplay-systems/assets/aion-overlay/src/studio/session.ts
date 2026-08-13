export type Chapter = {
  id: string;
  name: string;
  until: number;
  status: string;
  fog?: string;
  sun?: string;
  ground?: string;
};

export type Session = {
  title: string;
  objective: string;
  chapters: Chapter[];
  look: { sky: string; ground: string; icon: string };
  models?: {
    player?: { file: string; walk?: string; run?: string; height?: number };
    enemy?: { file: string; walk?: string; run?: string; height?: number };
    pickup?: { file: string };
  };
};

export function defaultSession(): Session {
  return {
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
  };
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
