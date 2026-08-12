---
name: threejs-game-ui-designer
description: "Design premium Three.js game UI. Use for HUDs, menus, overlays, pause/win/lose screens, settings, icon controls, touch UI, typography, responsive layout, safe areas, text fit, and UI/world cohesion."
---

# Three.js Game UI Designer

## Purpose

Make game UI intentional, readable, responsive, and genre-specific.

## Workflow

Load `references/ui-patterns.md` as the first action when designing HUDs, menus, overlays, pause/win/lose screens, settings, touch controls, typography, responsive layout, safe areas, text fit, icons, or UI/world cohesion. Track it in a reference ledger with yes/no, path, and failure reason. Do not mark the UI phase complete while this reference is skipped for interface work.

Load `references/checklists/game-ui-quality.md`, `references/checklists/hud-readability.md`, and `references/checklists/responsive-ui-fit.md` before claiming UI/HUD/menu work is complete. Load `references/checklists/mobile-input.md` when touch controls or mobile safe areas are in scope.

Load `references/prompt-templates.md` only when the user asks for reusable prompts, a UI pass prompt, or a task template.

Load `threejs-image-generator` when logos, icons, GUI art, faction marks, menu backgrounds, decals, or 2D HUD assets would improve quality. Use `threejs-3d-generator` only for 3D menu/showcase objects or diegetic 3D UI props, not normal flat HUD elements.

1. Capture/inspect desktop and mobile screenshots.
2. Inventory UI states: gameplay, pause, settings, fail/retry, win/milestone, loading, touch controls.
3. Define hierarchy: survival/status, objective, feedback, flavor.
4. Replace utility stat cards with authored clusters, meters, badges, icons, alerts, and modal states.
5. Use stable dimensions, safe-area padding, text-fit constraints, hover/pressed/focus/disabled states.
6. Wire UI to game state, not duplicated rules.
7. Verify text fit, overlap, safe areas, touch targets, responsive screenshots, and real state changes.

## Common Failure Modes

- Generic dashboard/stat-card HUD.
- UI covers player/threats.
- Text shifts/clips on mobile.
- Decorative panels reduce readability.
- Touch controls look right but do not emit intents.

## Final Response

Report the reference ledger, UI state checklist, UI intent, states covered, files changed, screenshots, text-fit/overlap checks, safe-area/touch-target evidence, controls, and remaining risks.
