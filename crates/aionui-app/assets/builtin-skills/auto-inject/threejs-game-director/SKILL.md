---
name: threejs-game-director
description: "Primary entrypoint for complete Three.js browser game creation and premium iteration. Use by default for build-a-game, upgrade, polish, premium, AAA, high-fidelity, showcase, from-scratch, endless runner, arcade, action, or release-ready requests. Orchestrates sibling skills for gameplay, AAA graphics, UI, debug/profile, and QA/release, plus 3D/image/audio generators for characters, vehicles, weapons, buildings, props, skies, textures, logos, icons, GUI art, and SFX/voice. Keeps skill-loading, reference, asset-sourcing, and phase ledgers so users never choose skills manually."
---

# Three.js Game Director

## Purpose

Own the end-to-end game outcome. Build the playable loop, route through the right phases, verify evidence, and do not call prototype-quality work complete. Broad "make a game" requests share one experience-intent contract across phases: player promise, emotion arc, content ledger, and audiovisual states. "Less basic" from the user means the current visual level is rejected: treat it as the premium bar.

## Runner Capability Check

Before planning, note what this runner can do and adapt:

1. **Invoke sibling skills directly?** Usually not — the runner invokes only this skill. Load sibling `SKILL.md` files with file-read tools instead. Never claim a skill was "invoked" when it was only loaded/read.
2. **Read files by path?** Resolve every skill and reference path through the path ladder below. If a required file cannot be read anywhere on the ladder, record the failure in the ledger and use `references/phase-playbook.md` as the fallback procedure for that phase.
3. **Run shell commands (node)?** If yes, use the packaged Node scripts (scaffold creator, `launch_game.mjs`, canvas inspector, report audit). `python3` is optional fallback only when `node` is missing from PATH or the Node command exits non-zero. Exit 0 with empty stdout is a script bug — retry Node or stop; do not switch to Python. Never run bare `python` (Windows Store stub). If node/npm are missing from the shell, say so; do not tell the user to install Node — AionUi provides a managed runtime on Windows, macOS, and Linux.
4. **Drive a browser / run Playwright?** If yes, capture screenshots and canvas inspection yourself. If not, ask the user to run `npm run verify:visual` and `npm run inspect:canvas` and paste the results; report unverified visuals as a residual risk, never as verified.

### Skill Path Ladder

Try in order, expanding `~` to the user's home directory when the read tool requires absolute paths:

1. `../<skill-name>/SKILL.md` relative to this skill's directory
2. `.aionrs/skills/<skill-name>/SKILL.md` in the workspace
3. `~/.claude/skills/<skill-name>/SKILL.md`
4. `~/.codex/skills/<skill-name>/SKILL.md`
5. `~/.agents/skills/<skill-name>/SKILL.md`
6. `skills/<skill-name>/SKILL.md` in the repository source

Reference files resolve the same way: `<skill-dir>/references/<file>.md`. Sibling skills point back to this ladder instead of restating it.

## Sibling Skill Loading

For a new game, pick a frozen cartridge then fill look/cast/audio. Load only:

1. `threejs-gameplay-systems/SKILL.md` — `create_threejs_game.mjs --cartridge collect|jump`
2. `threejs-image-generator/SKILL.md` — sky / ground / HUD icon into `public/look/`
3. `threejs-audio-generator/SKILL.md` — one `kit` command
4. `threejs-3d-generator/SKILL.md` — one `cast` into `public/look` + `look.json` models

Do not load `threejs-aaa-graphics-builder`, `threejs-game-ui-designer`, `threejs-debug-profiler`, or `threejs-qa-release` on the first game unless the user asked to polish an already delivered cartridge. Do not rewrite overlay `Game.ts`, `Player.ts`, `Pickup.ts`, `LookSystem.ts`, `AudioSystem.ts`, `PauseShare.ts`, `WorldKit.ts`, or `studio/cast.ts`. Chapters, density, threat, title, and cartridge belong in `public/look/look.json` only.

Cartridge routing: jump / platform / 跳跃 / 平台 → `--cartridge jump`. Otherwise `--cartridge collect`. Never invent a third loop in source.

For narrow director-invoked work, load the directly relevant sibling plus `threejs-qa-release` when verification is in scope.

## External Asset Sourcing Gate

- Never record "not-needed" for a generator before loading its `SKILL.md` when trigger surfaces exist.
- Before claiming an API key is unavailable, run the credential probe and paste its literal `KEY=SET|MISSING` output into the report. Each generator script also has its own `probe` subcommand.

```bash
node <director-skill-dir>/scripts/probe_asset_credentials.mjs
```

Prefer the Node probe (works on Windows without bash). `bash .../probe_asset_credentials.sh` is Unix fallback only.

- For 2D images in Aion: if `aionui_image_generation` is in the tool list, record `AIONUI_IMAGE_MCP=SET` and use that MCP via `threejs-image-generator`. `GEMINI_API_KEY=MISSING` is not a 2D skip/blocker in that case. Gemini/`uv` is fallback only when the MCP tool is absent or the MCP call failed.
- For 3D and audio in Aion: the desktop shell injects `TRIPO_API_KEY`, `ELEVENLABS_API_KEY`, and `SEED_TTS_API_KEY` into the agent process. Run the Node probe. Treat `SET` as the default path: generate with `threejs-3d-generator` and `threejs-audio-generator`. Do not skip those generators because you assume keys are missing. Procedural fallback is allowed only after a literal `MISSING` line or a real API/network/quota error.

- For premium hero surfaces (player, enemy, boss, creature, vehicle, ship, weapon, building, signature prop), procedural-only is not an allowed final answer without real blocker evidence: a `MISSING` probe line, or an attempted generation command plus its API/network/quota error. Otherwise at least one high-value surface must show a 3D generator task ID, downloaded GLB/GLTF/FBX path, image generator output path, or documented hybrid chain.
- For premium active gameplay, missing audio is a reported gap unless the user asked for silent/offline output or the audio key/API is blocked.
- Fill the external asset sourcing ledger before the graphics phase. The ledger template and the allowed skip reasons live in `references/phase-playbook.md`.

## Reference Gate

References are phase-entry gates, not optional enrichment. The canonical per-phase Required References list lives in `references/phase-playbook.md`; load that file at planning time for broad work and at phase entry otherwise.

- Load required references at phase entry, not at the end.
- Track every required reference in the reference ledger with yes/no/not-needed, path, and failure reason.
- A phase cannot be marked `done` until its required references are loaded, or the final answer reports the reference as unavailable and the phase as blocked/fallback.
- For premium/AAA/showcase claims, the final response must include the filled 10-category visual scorecard from `threejs-aaa-graphics-builder/references/visual-scorecard.md`, including measured evidence, average, and automatic failures remaining. Do not substitute a personal rubric.
- Thorough mode is the default for broad, premium, AAA, showcase, complete, and release-ready requests. Economy mode is allowed only for narrow fixes that do not claim premium quality.

If Task/subagent/workflow tools are available, delegate each major phase to a focused worker with the phase `SKILL.md` plus its required references explicitly loaded. If unavailable, execute serially after loading the same files.

## Ledgers

Keep four ledgers: skill-loading, reference, external asset sourcing, and phase execution. Templates live in `references/phase-playbook.md`.

Compaction rule: report every row that has meaningful state (yes/no/blocked/done/skipped plus path or evidence), and collapse consecutive `not-needed` rows into a single line naming them. Never omit or compress rows that carry real state.

## Phase Routing

- `threejs-gameplay-systems`: design brief, core loop contract, level/encounter plan, first playable slice, architecture, mechanics, entities, input, camera, physics selection, game feel.
- External asset sourcing: credential probe, generator skill loading, source decision per surface, task IDs/output files or blocker evidence. Must complete before the graphics phase is `done` for premium work.
- `threejs-aaa-graphics-builder`: basic-looking screenshots, asset architecture, models, materials, technical art, shaders, VFX, lighting/render, visual scorecard.
- `threejs-game-ui-designer`: HUDs, menus, overlays, responsive UI, icons, safe areas, UI states.
- `threejs-debug-profiler`: blank canvas, render/runtime bugs, loading, resize, mobile input/render bugs, performance profiling.
- `threejs-qa-release`: browser QA, screenshots, canvas pixels, responsive checks, visual test harness decision, bot playtest, production build, preview, release notes.
- `threejs-3d-generator` / `threejs-image-generator` / `threejs-audio-generator`: external AI-generated 3D models and rigs, 2D concepts/textures/logos/GUI art, and SFX/ambience/voice.

When a sibling skill file is loaded, follow its workflow for that phase. Phase entry/exit evidence, ledger templates, and the fallback procedure for unloadable siblings all live in `references/phase-playbook.md`.

## Packaged Runtime Resources

New projects use the gameplay skill's scaffold creator; canvas verification uses the generated game's `npm run inspect:canvas` or the QA skill's packaged inspector:

```bash
node <threejs-gameplay-systems-skill-dir>/scripts/create_threejs_game.mjs ./my-game --cartridge collect
cd ./my-game && npm install
node <threejs-gameplay-systems-skill-dir>/scripts/launch_game.mjs ./my-game --no-open
```

Use `launch_game.mjs` (not foreground `npm run play`): Vite must be detached or ExecCommand times out and kills the server. Give `npm install` a long timeout. Pass `--cartridge jump` when the user asked for a platformer.

## Execution order

Do not skip to player handoff. Do not rewrite overlay gameplay files.

1. **Make** the cartridge: scaffold with `--cartridge collect|jump`, fill sky/ground/icon, audio `kit`, `cast` models, and `look.json` title/objective/density/threat/chapters. First `LAUNCH_OK` is a checkpoint. Chapter QA, screenshots, and Playwright use `--no-open` only.
2. **Prove it plays** before `--deliver`: `npm run build`; `launch_game.mjs --no-open`; actually drive main input, fail/retry, pause, settle, and share; console clean; screenshots; non-blank canvas.
3. **Hand off** only the playtested game:

```bash
node <threejs-gameplay-systems-skill-dir>/scripts/launch_game.mjs ./my-game --deliver
```

Require `GAME_DELIVERED dist=<abs>/dist`. AionUi mounts that folder in the in-app preview. Do not start Vite, occupy port 5188, open the system browser, or tell the user to click a link. Do not `--deliver` until step 2 passed.

After scaffold create, apply generated sky/ground/icon, then generate audio with the Node kit (not foreground Python, not twenty SFX prompts). Voice follows the scene: lyrics/song → vocals; 旁白/对白/announcer → TTS; otherwise instrumental + SFX only.

```bash
node <threejs-gameplay-systems-skill-dir>/scripts/apply_look.mjs \
  --out ./my-game --title "<title>" \
  --sky <sky> --ground <ground> --icon <icon>
```

Then generate T-pose concepts and run **one** `cast` into `public/look` + `look.json` models. Prefer `--player-image` / `--pickup-image` (image-to-3D). First-person may omit `--player-image` but must still `--enemy-image` for every visible character. Do not pass `--animate-in-place`. Overlay `Player`/`Pickup` load those files; kitbash stays as fallback.

```bash
node <threejs-3d-generator-skill-dir>/scripts/threejs_3d_asset.mjs probe
node <threejs-3d-generator-skill-dir>/scripts/threejs_3d_asset.mjs cast \
  --out ./my-game \
  --player-image assets/concepts/player-tpose.png \
  --pickup-image assets/concepts/pickup.png
```

`--deliver` audits the **game source and look.json files**, not the markdown report. Cones/capsules/icosahedrons in `src/`, or missing model files, print `ART_FAIL` and never `GAME_DELIVERED`.

```bash
node <threejs-audio-generator-skill-dir>/scripts/threejs_audio_asset.mjs probe
node <threejs-audio-generator-skill-dir>/scripts/threejs_audio_asset.mjs kit \
  --genre "<english genre>" --emotion "<english emotions>" \
  --verb "<experience-intent verb>" \
  --explore "<heard space, materials>" \
  --pressure "<escalation hearables>" \
  --settle "<aftertaste hearables>" \
  --spoken "no dialogue" \
  --voice auto --out ./my-game
```

Pass the beat sheet as `--explore/--pressure/--settle` (or one `--scene` that contains them). If anyone speaks, add `--spoken "旁白"` and `--lines "a|b"`. Do not call kit with only `--genre arcade`. Overlay `AudioSystem` plays kit SFX/TTS from diagnostics and input; do not leave generated files unplayed.

## Premium Completion Rule

Premium, AAA, polished, complete, release-ready, and showcase requests require visible quality across gameplay, hero/player, obstacles/enemies, rewards/interactables, world kit, HUD/menu states, render/lighting/materials, feel, performance/mobile, and QA. If screenshots are dominated by primitives, flat roads/arenas, generic stat cards, sparse worlds, or glow-only detail, the task is not done. The full completion gate is in `references/phase-playbook.md`.

## Required Verification

- Build/typecheck; chapter/QA launches use `launch_game.mjs --no-open` (`LAUNCH_OK`). Prove the game plays before the last command `launch_game.mjs --deliver` (`GAME_DELIVERED dist=`). Never `--deliver` an untested build. Never foreground `npm run play`. Never rewrite overlay `Game.ts` to invent a new loop.
- Game design brief (including experience intent), core loop contract, emotion beat sheet, chapter ledger, and level/encounter plan for broad game creation or major gameplay changes.
- Active desktop and mobile screenshots plus nonblank canvas pixel evidence.
- Main input/objective/fail-or-restart path exercised through named chapters, not only the first minute.
- Hero/player is not a capsule/cube/primitive-plus-glow in the final delivery. `--deliver` is blocked (`ART_FAIL`) until `cast` wrote look.json model files and `src/` has no Cone/Capsule/Icosahedron/Tetrahedron meshes. First-person may omit `player.glb` but must ship `enemy.glb` for visible characters.
- Audio matrix with emotion-driven music states (or a documented blocker); not one unchanging loop unless the genre intent is a single restrained bed with contrast.
- Share control on pause/settle; `localhost` reported as a local playtest URL only.
- Visual scorecard with measured evidence for premium/AAA claims, plus a fresh-eyes review pass per `threejs-aaa-graphics-builder/references/visual-scorecard.md`.
- External asset sourcing ledger, credential probe output, and real external outputs or blocker evidence for premium asset-category claims.
- Renderer diagnostics when graphics changed; technical art budget and VFX/readability evidence when premium graphics changed.
- Visual test harness decision, and bot playtest evidence when release-ready gameplay is claimed.
- Fresh-eyes note: expected emotion → observed behavior/feedback → delta → fix. Do not claim user-measured retention without a real player test.
- Final ledgers with evidence and remaining blockers.

## Report Audit

When shell tools are available, draft the final evidence report to a markdown file and audit it before finalizing broad or premium work:

```bash
node <director-skill-dir>/scripts/audit_reference_report.mjs /path/to/final-report.md
```

Prefer the `.mjs` auditor. Fall back to `python3 .../audit_reference_report.py` only when `node` is missing or Node exits non-zero. Exit 0 with empty stdout is not a fallback reason. Never run bare `python`.

Default (no flag) already requires experience intent, emotion beat, chapter ledger, share, play/launch, and music/audio state. Use `--premium` for premium/AAA/showcase/high-fidelity/polished/complete/release-ready/"less basic" claims; add `--physics` for physics-heavy games; add `--audio` when generated or integrated audio is in scope; add `--no-design` only for debug/perf/QA-only reports with no gameplay claims. If the audit fails, fix the missing sections or state the exact blocker instead of claiming completion. If the script is unavailable, manually enforce the same sections listed in Required Verification.

## Final Response

Lead with the user-facing close in plain language: the game is open (or how to open it), controls, current goal, how to enable audio, and where share lives. Do not start with an npm command. Do not name reference titles or claim a production grade.

Then report the ledgers (compacted per the rule above), experience intent, game design brief, core loop contract, emotion beat sheet, level/encounter plan, files changed, run URL (label `localhost` as local playtest), controls, verification commands, screenshots/artifacts, renderer/performance notes, technical art budget, visual test harness decision, quality gates passed, skipped phases, share status, and remaining risks. For premium/AAA/showcase claims, include the filled visual scorecard with measured evidence and automatic failures remaining. Be precise: "invoked" means a slash/tool skill invocation; "loaded" means the file was read into context; "executed phase" means the work was performed under loaded skill guidance or the phase playbook.
