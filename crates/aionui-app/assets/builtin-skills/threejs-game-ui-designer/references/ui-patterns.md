# Game UI Patterns

Use this when designing HUDs, menus, overlays, pause/fail/win states, touch controls, typography, responsive layout, and UI/world cohesion.

## UI Principles

- Build the game interface, not a web dashboard.
- Prioritize gameplay hierarchy: survival/status, objective/progress, immediate feedback, then flavor.
- Use meters, icons, reticles, badges, alert strips, cooldown rings, inventory slots, minimaps, diegetic labels, and compact clusters before generic stat cards.
- Keep UI outside the play path and away from threats, pickups, the player, and the next decision.
- UI should reinforce the world art direction through material cues, color roles, icon shapes, and motion language.
- Do not use visible text to explain obvious controls when an icon, affordance, or direct interaction can do the job.

## Required States

Inventory states before designing:

- Gameplay HUD.
- Pause/resume.
- Settings or audio/accessibility controls when useful.
- Fail/retry.
- Win/milestone/level complete when relevant.
- Loading/empty/error when async assets exist.
- Mobile/touch controls when target includes mobile.
- Debug/tuning UI gated separately from player UI.

Premium games should not have only one HUD state.

## HUD Composition

Use intentional zones:

- Top or top-left: objective, wave, distance, timer, route/progress.
- Top or top-right: score, currency, combo, inventory, pause.
- Bottom left/right: touch movement/action controls when needed.
- Center top or near player: short event banners, combo, warnings.
- Near-world labels: diegetic prompts, target markers, offscreen indicators.

Rules:

- Use fixed-width numeric containers for score, timer, ammo, speed, health, and best values.
- Use icons plus short labels for unfamiliar resources.
- Use meter fills for quantities the player must read quickly.
- Use alert colors consistently: danger, reward, shield, boost, objective, disabled.
- Animate state changes briefly: count-up, meter fill, pulse, slide/fade, snap, ring cooldown.
- Do not stack multiple large banners over the play path.

## Menus And Overlays

Pause/fail/win overlays should support quick action:

- Primary action first: resume, retry, continue, next.
- Secondary actions: settings, quit, restart, level select.
- Avoid marketing-page hero layouts inside a game.
- Keep menu panels stable and readable across desktop/mobile.
- Use icon buttons for pause, sound, restart, fullscreen, settings when familiar.
- Provide focus/hover/pressed/disabled states.
- Gate debug panels behind a dev flag or query param.

## Touch Controls

When mobile is in scope:

- Use pointer events where possible.
- Ensure controls emit the same game intents as keyboard/mouse.
- Handle `pointerup`, `pointercancel`, `lostpointercapture`, blur, and visibility change.
- Use safe-area insets.
- Avoid controls overlapping HUD warnings or the play path.
- Keep touch targets at least roughly 44 CSS pixels where practical.
- Separate adjacent controls enough to prevent accidental presses.
- Use `touch-action` to prevent unwanted page scrolling only in control regions or the game surface.

## Responsive Constraints

- Define stable dimensions with CSS variables, `clamp`, grid tracks, fixed icon slots, and fixed-width numbers.
- Do not scale text purely with viewport width.
- Avoid negative letter spacing.
- Check desktop, laptop, tablet/narrow, and phone viewports.
- Test longest likely values: high score, long labels, multi-digit timers, localized-ish text if relevant.
- No clipped text, overlapping controls, unreadably small labels, or layout shift from changing values.
- Menus must remain reachable without offscreen controls.

## Visual Style

- Match the genre: arcade racers need speed/status readability; fighters need health/round/impact hierarchy; exploration games need inventory/objective clarity.
- Prefer restrained panels with meaningful geometry, borders, ticks, glow accents, and material cues over nested cards.
- Use a limited status palette plus neutral surfaces.
- Avoid one-note purple/blue gradient UI unless it is strongly justified by the game world.
- Connect UI motifs to world decals, faction marks, vehicle panels, pickups, or hazards.

## 2D Asset Generation

Use `threejs-image-generator` for 2D interface assets when hand-coded CSS/icons are not enough:

- Faction logos, team crests, title marks.
- Pickup, ability, weapon, inventory, achievement, and objective icons.
- Hazard signs, decals, lane glyphs, cockpit labels, item badges.
- Menu/loading/background plates, illustrated map panels, world-style UI textures.
- GUI material references: glass panels, metal frames, holographic strips, paper/parchment, tactical screens.

Use `threejs-3d-generator` only when the UI needs a 3D object: rotating character preview, vehicle garage, weapon inspect model, trophy, diorama, or diegetic 3D menu prop.

## State Wiring

- UI reads game state from a single source of truth.
- UI events dispatch game intents; they should not mutate unrelated simulation internals directly.
- UI should update on pause, restart, resize, mobile orientation, mute, fail/win, score, health, boost, combo, inventory, and accessibility settings.
- Avoid stale values after restart.

## Verification

Capture evidence:

- Gameplay HUD desktop screenshot.
- Gameplay HUD mobile screenshot when in scope.
- Pause/fail/retry state screenshot if changed.
- Text-fit and overlap check with high values.
- Touch target and safe-area check when mobile is in scope.
- Interaction test for UI buttons and touch controls.
- Console/page error check after UI events.
- Generated 2D asset path and prompt when `threejs-image-generator` was used.
- Imported 3D preview model path and renderer diagnostics when `threejs-3d-generator` was used.

## Common Failures

- Generic dashboard/stat-card HUD.
- Nested cards and oversized decorative panels.
- UI covers threats, pickups, player, or next decision.
- Text explains obvious controls instead of designing affordances.
- Mobile safe areas ignored.
- Touch controls look correct but do not emit intents.
- Values change width and shift layout during play.
- Debug UI ships as player UI.
