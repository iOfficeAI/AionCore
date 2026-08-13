#!/usr/bin/env node
/** Copy generated sky/ground/icon into public/look and write look.json. */

import fs from 'node:fs';
import path from 'node:path';
import { defaultSession, lookPaths } from './studio_session_lib.mjs';

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
