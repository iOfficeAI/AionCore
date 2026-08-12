# Three.js Game UI Prompt Templates

Reusable prompt templates packaged with this skill. Use only templates relevant to the current request, and adapt placeholders to the game/project context.

---

# Premium HUD/UI Pass Prompt

Use `threejs-game-ui-designer` to make this Three.js game's HUD and in-game UI feel premium, readable, and genre-specific.

Context to gather:
- Current screenshots at desktop and mobile sizes.
- Game genre, core verb, target mood, and player decisions the HUD must support.
- Current HUD/menu files, game state sources, input model, and touch controls.

Design goals:
- Keep the playable game as the first screen.
- Make survival/status/objective information readable during motion.
- Use stable dimensions for counters, buttons, meters, and labels.
- Use icons where they reduce clutter, with labels where meaning is ambiguous.
- Match the game's world materials, color, typography, and feedback language.

Constraints:
- Do not add marketing-page hero sections or explanatory feature copy.
- Do not nest cards inside cards.
- Do not let UI cover critical gameplay at desktop or mobile sizes.
- Avoid text overflow, clipped controls, layout shift, and generic dashboard styling.

Verification:
- Build and run locally.
- Capture desktop and mobile screenshots.
- Check console/page errors.
- Verify text fit, no overlap, no clipped controls, and at least one UI state change from real input.

---

# Responsive Game Menu Pass Prompt

Use `threejs-game-ui-designer` to design or improve this Three.js game's pause, start, settings, win, lose, or restart menus.

Menu states needed:
- 

Target devices/orientations:
- 

Requirements:
- Menus must feel like part of the game, not a website overlay.
- Modal menus may pause or dim gameplay, but should not look like marketing cards.
- Buttons need clear hover, pressed, focus, disabled, and touch states.
- Settings must use appropriate controls: toggles, sliders, segmented controls, icon buttons, and select menus.
- Layout must respect safe-area insets and remain usable on mobile.

Implementation notes:
- Prefer semantic HTML/CSS over canvas-rendered UI unless 3D placement is required.
- Keep UI state driven by the game state model.
- Keep dimensions stable so labels and counters do not shift layout.

Verification:
- Test every menu state.
- Capture desktop and mobile screenshots.
- Check keyboard/mouse and touch paths when both are supported.
- Confirm no text overflow, overlap, clipping, or unreachable controls.
