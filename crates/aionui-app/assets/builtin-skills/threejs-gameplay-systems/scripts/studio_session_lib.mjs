/** Pure helpers for first-game chapters, look paths, and share copy. No network. */

export function defaultSession() {
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
  };
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
