#!/usr/bin/env node
/** Copy generated sky/ground/icon into public/look and write look.json. */

import fs from 'node:fs';
import path from 'node:path';
import { defaultSession, lookPaths, mergeCastModels, modelLookPaths } from './studio_session_lib.mjs';

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    if (!token.startsWith('--')) continue;
    const key = token.slice(2);
    const value = argv[i + 1];
    if (!value || value.startsWith('--')) args[key] = true;
    else {
      args[key] = value;
      i += 1;
    }
  }
  return args;
}

function copyLookFile(source, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(source, dest);
}

function main(argv) {
  const args = parseArgs(argv);
  if (!args.out) {
    throw new Error('apply_look.mjs requires --out <game-dir>');
  }
  const root = path.resolve(args.out);
  const publicLook = path.join(root, 'public', 'look');
  const kitPath = path.join(publicLook, 'look.json');
  const session = fs.existsSync(kitPath)
    ? { ...defaultSession(), ...JSON.parse(fs.readFileSync(kitPath, 'utf8')) }
    : defaultSession();
  if (args.title) session.title = args.title;
  if (args.objective) session.objective = args.objective;

  const paths = lookPaths({
    sky: args.sky,
    ground: args.ground,
    icon: args.icon,
  });
  session.look = { ...session.look, ...paths };

  if (args.sky) copyLookFile(path.resolve(args.sky), path.join(root, 'public', paths.sky));
  if (args.ground) copyLookFile(path.resolve(args.ground), path.join(root, 'public', paths.ground));
  if (args.icon) copyLookFile(path.resolve(args.icon), path.join(root, 'public', paths.icon));

  const modelInput = {
    player: args.player,
    playerWalk: args['player-walk'],
    playerRun: args['player-run'],
    enemy: args.enemy,
    enemyWalk: args['enemy-walk'],
    enemyRun: args['enemy-run'],
    pickup: args.pickup,
  };
  const modelDest = modelLookPaths(modelInput);
  const models = { ...(session.models || {}) };
  if (modelDest.player && args.player) {
    copyLookFile(path.resolve(args.player), path.join(root, 'public', modelDest.player));
    models.player = {
      ...(models.player || {}),
      file: modelDest.player,
      ...(modelDest.playerWalk ? { walk: modelDest.playerWalk } : {}),
      ...(modelDest.playerRun ? { run: modelDest.playerRun } : {}),
    };
  }
  if (modelDest.playerWalk && args['player-walk']) {
    copyLookFile(path.resolve(args['player-walk']), path.join(root, 'public', modelDest.playerWalk));
    if (models.player) models.player.walk = modelDest.playerWalk;
  }
  if (modelDest.playerRun && args['player-run']) {
    copyLookFile(path.resolve(args['player-run']), path.join(root, 'public', modelDest.playerRun));
    if (models.player) models.player.run = modelDest.playerRun;
  }
  if (modelDest.enemy && args.enemy) {
    copyLookFile(path.resolve(args.enemy), path.join(root, 'public', modelDest.enemy));
    models.enemy = {
      ...(models.enemy || {}),
      file: modelDest.enemy,
      ...(modelDest.enemyWalk ? { walk: modelDest.enemyWalk } : {}),
      ...(modelDest.enemyRun ? { run: modelDest.enemyRun } : {}),
    };
  }
  if (modelDest.pickup && args.pickup) {
    copyLookFile(path.resolve(args.pickup), path.join(root, 'public', modelDest.pickup));
    models.pickup = { file: modelDest.pickup };
  }
  Object.assign(session, mergeCastModels(session, models));

  fs.mkdirSync(publicLook, { recursive: true });
  fs.writeFileSync(kitPath, `${JSON.stringify(session, null, 2)}\n`);
  console.log(`LOOK_OK path=${kitPath}`);
  console.log(`LOOK_SKY=${session.look.sky}`);
  console.log(`LOOK_GROUND=${session.look.ground}`);
  console.log(`LOOK_ICON=${session.look.icon}`);
}

try {
  main(process.argv.slice(2));
} catch (error) {
  console.error(`apply_look.mjs: ${error instanceof Error ? error.message : error}`);
  process.exit(1);
}
