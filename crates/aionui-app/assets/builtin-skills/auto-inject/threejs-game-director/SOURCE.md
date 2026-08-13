# Vendored Three.js Game Skills

Source: https://github.com/majidmanzarpour/threejs-game-skills
License: MIT (see `LICENSE` in this directory)
Vendored: 2026-08-12 from `main` (`skills/` only)

These nine skills ship as Aion built-in corpus under
`crates/aionui-app/assets/builtin-skills/auto-inject/threejs-*`. They are
compiled into `aioncore` and materialized at startup. Users do not install
them.

They live under `auto-inject/` so every new conversation gets them in the
skill index. Agents load a full skill body mid-turn with
`[LOAD_SKILL: threejs-game-director]` (or a sibling) when the task is a
browser / Three.js / WebGL game.

`aionui-assistant`, `game-dev-studio`, and `game-3d` still list them in
`enabled_skills` so a per-assistant auto-inject exclude cannot drop the pack
from those presets. Engine-specific spatial experts do not list them in
`enabled_skills`. Assistant rules still forbid loading this pack for Unity /
Unreal / Godot / Roblox / XR / board / Office work.

Aion injects `TRIPO_API_KEY`, `ELEVENLABS_API_KEY`, and `SEED_TTS_API_KEY` into agent sessions
(from `AIONUI_BUILTIN_TRIPO_API_KEY` / `AIONUI_BUILTIN_ELEVENLABS_API_KEY` /
`AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY`).
2D images use the built-in `aionui_image_generation` MCP. Probe first; do not
treat 3D/audio as missing by default.
