# Vendored Three.js Game Skills

Source: https://github.com/majidmanzarpour/threejs-game-skills
License: MIT (see `LICENSE` in this directory)
Vendored: 2026-08-12 from `main` (`skills/` only)

These nine skills ship as Aion built-in corpus under
`crates/aionui-app/assets/builtin-skills/threejs-*`. They are compiled into
`aioncore` and materialized at startup. Users do not install them.

Default-enabled for assistants: `aionui-assistant` (butler / Guid default),
`game-dev-studio`, and `game-3d` via `enabled_skills` in
`builtin-assistants/assistants.json`. Engine-specific spatial experts do not
default-enable these skills. The butler rule only loads the director when the
task is explicitly a browser / Three.js / WebGL game.

Aion injects `TRIPO_API_KEY` and `ELEVENLABS_API_KEY` into agent sessions
(from `AIONUI_BUILTIN_TRIPO_API_KEY` / `AIONUI_BUILTIN_ELEVENLABS_API_KEY`).
2D images use the built-in `aionui_image_generation` MCP. Probe first; do not
treat 3D/audio as missing by default.

Do not put these under `auto-inject/`. They are Three.js-specific.
