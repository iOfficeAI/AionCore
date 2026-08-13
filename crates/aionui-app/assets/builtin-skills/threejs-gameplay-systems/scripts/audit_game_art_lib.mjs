/** Scan the game source and look.json model files. Does not read markdown reports. */

import fs from 'node:fs';
import path from 'node:path';

const FORBIDDEN_PRIMITIVE =
  /\b(?:new\s+(?:THREE\.)?)?(Cone|Capsule|Icosahedron|Tetrahedron)Geometry\s*\(/;
const MODEL_EXTS = /\.(glb|gltf|fbx)$/i;

function listSourceFiles(dir, acc = []) {
  if (!fs.existsSync(dir)) return acc;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name === 'tests') continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      listSourceFiles(full, acc);
      continue;
    }
    if (!entry.name.endsWith('.ts')) continue;
    if (entry.name.endsWith('.d.ts') || entry.name.includes('.test.') || entry.name.includes('.spec.')) {
      continue;
    }
    acc.push(full);
  }
  return acc;
}

function resolvePublicFile(gameDir, file) {
  const rel = String(file || '').replace(/^\//, '');
  if (!rel) return null;
  const candidates = [path.join(gameDir, 'public', rel), path.join(gameDir, rel)];
  return candidates.find((candidate) => fs.existsSync(candidate)) || null;
}

function readModels(gameDir, failures) {
  const lookPath = path.join(gameDir, 'public', 'look', 'look.json');
  if (!fs.existsSync(lookPath)) {
    failures.push('public/look/look.json missing');
    return {};
  }
  try {
    const data = JSON.parse(fs.readFileSync(lookPath, 'utf8'));
    return data.models && typeof data.models === 'object' ? data.models : {};
  } catch {
    failures.push('public/look/look.json is not valid JSON');
    return {};
  }
}

function requireSlot(gameDir, models, slot, failures) {
  const spec = models[slot];
  const file = spec?.file;
  if (!file) {
    failures.push(`models.${slot}.file missing`);
    return;
  }
  if (!MODEL_EXTS.test(file)) {
    failures.push(`models.${slot}.file ${file} is not glb/gltf/fbx`);
  }
  if (!resolvePublicFile(gameDir, file)) {
    failures.push(`models.${slot}.file ${file} not found`);
  }
  for (const extra of ['walk', 'run']) {
    const extraFile = spec?.[extra];
    if (!extraFile) continue;
    if (!resolvePublicFile(gameDir, extraFile)) {
      failures.push(`models.${slot}.${extra} ${extraFile} not found`);
    }
  }
}

export function auditGameArt(gameDir) {
  const root = path.resolve(gameDir);
  const failures = [];

  for (const file of listSourceFiles(path.join(root, 'src'))) {
    const rel = path.relative(root, file);
    const lines = fs.readFileSync(file, 'utf8').split('\n');
    lines.forEach((line, index) => {
      const match = FORBIDDEN_PRIMITIVE.exec(line);
      if (match) {
        failures.push(`${rel}:${index + 1} uses ${match[1]}Geometry`);
      }
    });
  }

  const models = readModels(root, failures);
  const entitiesDir = path.join(root, 'src', 'entities');
  const entityFiles = fs.existsSync(entitiesDir)
    ? fs.readdirSync(entitiesDir).filter((name) => name.endsWith('.ts'))
    : [];
  const extraEntities = entityFiles.filter((name) => name !== 'Player.ts' && name !== 'Pickup.ts');

  if (entityFiles.includes('Pickup.ts')) requireSlot(root, models, 'pickup', failures);
  if (extraEntities.length) requireSlot(root, models, 'enemy', failures);
  if (models.player?.file || !extraEntities.length) {
    requireSlot(root, models, 'player', failures);
  }

  return { ok: failures.length === 0, failures };
}
