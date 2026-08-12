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

Do not put these under `auto-inject/`. They are Three.js-specific.
