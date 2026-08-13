---
name: threejs-audio-generator
description: "Generate first-game audio kits and individual assets for Three.js browser games. Music and SFX use ElevenLabs; spoken lines use Volcengine seed-tts-2.0. Prefer the Node kit command: one score with explore/pressure/settle regions, event SFX, and scene-aware vocals or TTS. Fall back to python3 only when node is missing or Node exits non-zero."
---

# Three.js Audio Generator

## Purpose

Create game-ready audio for Three.js projects. First complete games use one `kit` command, not twenty invented prompts. Music and SFX: ElevenLabs. Spoken TTS: Volcengine `seed-tts-2.0` (`https://openspeech.bytedance.com/api/v3/plan/tts/unidirectional`). Never put API keys in skill files or browser/game code.

Resolve `<this-skill-dir>` in this order: `.aionrs/skills/threejs-audio-generator`, `~/.claude/skills/threejs-audio-generator`, `~/.codex/skills/threejs-audio-generator`, `~/.agents/skills/threejs-audio-generator`, or repo `skills/threejs-audio-generator`.

## First Game Path

1. Probe with Node. Paste the literal `ELEVENLABS_API_KEY=SET|MISSING` and `SEED_TTS_API_KEY=SET|MISSING` lines. In Aion the desktop shell injects both; expect `SET`. Do not skip the kit because you assume a key is missing.

```bash
node <this-skill-dir>/scripts/threejs_audio_asset.mjs probe
```

Fall back to `python3 <this-skill-dir>/scripts/threejs_audio_asset.py probe` only when the shell reports `node` is not found, or Node exits non-zero with an error on stderr. Exit 0 with empty stdout is a script bug — retry the same Node command or stop; do not switch to Python. Never run bare `python`. If the director skill is loaded, you may use `threejs-game-director/scripts/probe_asset_credentials.mjs` for all three asset keys, then still run this Node probe for audio.

2. Decide voice from the **scene**, not a global default:

- Need sung vocals (歌词 / 主题曲 / lyrics / choir) → music may include vocals.
- Need spoken voice (旁白 / 对白 / announcer / narrator) → generate TTS lines; keep music instrumental unless the scene also sings.
- Otherwise → instrumental score + SFX only. Do not add vocals to fill time.

`--voice auto` (default) infers from `--genre --emotion --scene`. `--voice on|off` overrides.

3. Generate the kit into the game directory (writes `public/audio/...` for Vite). `--genre` is required, plus either `--scene` or `--explore --pressure --settle`. Write hearables in English when possible. Chinese is accepted for voice detection; ElevenLabs styles are English-only.

```bash
node <this-skill-dir>/scripts/threejs_audio_asset.mjs kit \
  --genre "cozy lantern ferry" \
  --emotion "safety, attachment" \
  --verb "carry light across water" \
  --explore "wooden dock at dusk, water lapping, lantern glass" \
  --pressure "crossing chop, lanterns dim" \
  --settle "far pier, warm glass" \
  --spoken "no dialogue" \
  --voice auto \
  --out ./my-game
```

If the scene has spoken lines, pass them:

```bash
node <this-skill-dir>/scripts/threejs_audio_asset.mjs kit \
  --genre "story climb" \
  --scene "旁白讲述身世" \
  --lines "你还记得上山的路。|不要回头。" \
  --out ./my-game
```

The kit writes one `audio/music/score.mp3` plus `kit.json` loop regions for `explore` / `pressure` / `settle`. Do not generate three unrelated songs. Overlay `AudioSystem` loads `/audio/kit.json` after user-gesture unlock, plays intro/settle TTS from `voice.lines[].cue`, and fires SFX from score/complete/speed/chapter diagnostics plus Space dash and Escape pause. If the key is missing or the API fails, report the blocker; the overlay still plays a procedural bed so the game is not silent. Spoken lines need `SEED_TTS_API_KEY=SET`; music/SFX need `ELEVENLABS_API_KEY=SET`.

`--dry-run` writes `kit.json` only (no API). Use it to confirm `VOICE=instrumental|vocals|tts|vocals+tts` before spending credits.

## Scene Voice Rule

| Scene need | Music | Speech |
| --- | --- | --- |
| Action, cozy, puzzle, horror without talk | Instrumental only | None |
| Ending theme, rhythm/lyrics, choir | Vocals allowed | Only if the scene also speaks |
| 旁白 / 对白 / announcer / tutorial voice | Instrumental | TTS lines |
| User said silent / offline | Skip generation | Skip |

Do not generate TTS because a kit exists. Do not put lyrics on a stealth or cozy bed unless the scene asks for singing.

## Individual Commands

Use these after the kit, for extra events only:

```bash
node <this-skill-dir>/scripts/threejs_audio_asset.mjs sfx \
  --prompt "short shield absorb, glassy hit, 0.8s tail, no music, no voice" \
  --duration 0.8 \
  --out assets/audio/sfx/shield.mp3

node <this-skill-dir>/scripts/threejs_audio_asset.mjs music \
  --genre "boss approach" \
  --scene "instrumental pressure" \
  --out assets/audio/music/boss.mp3

node <this-skill-dir>/scripts/threejs_audio_asset.mjs tts \
  --text "Perfect shot." \
  --out assets/audio/voice/perfect-shot.mp3
```

`python3 .../threejs_audio_asset.py` still covers `sfx`, `tts`, `isolate`, and `voice-change` when Node is missing or exited non-zero. Prefer Node for `probe`, `kit`, and `music`. Do not switch to Python because Node printed nothing and exited 0.

## Required Reference

Load `references/audio-workflows.md` when adding more than the first kit, wiring extra events, or claiming premium audio. First-kit-only work can proceed after this SKILL.md.

## Required Report

- Probe lines `ELEVENLABS_API_KEY=SET|MISSING` and `SEED_TTS_API_KEY=SET|MISSING` (and `--validate` if a present ElevenLabs key still fails).
- `VOICE=...` from the kit command.
- Paths: `public/audio/kit.json`, `public/audio/music/score.mp3`, SFX files, TTS files if any.
- Whether overlay `AudioSystem` is present (scaffold create applies it).
- Blocker if generation failed (HTTP/quota/network). Do not claim files exist that were not written.
