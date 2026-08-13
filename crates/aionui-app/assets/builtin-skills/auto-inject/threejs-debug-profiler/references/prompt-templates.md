# Three.js Debug/Profile Prompt Templates

Reusable prompt templates packaged with this skill. Use only templates relevant to the current request, and adapt placeholders to the game/project context.

---

# Mobile Input Prompt

Use `threejs-debug-profiler` for mobile render/input bugs, or `threejs-game-ui-designer` when the main issue is touch-control/HUD layout.

Target devices:
- 

Current issue:
- 

Requirements:
- Use pointer/touch handling that does not fight page scrolling.
- Keep HUD and controls inside safe areas.
- Verify portrait and landscape if both are supported.
- Check canvas sizing, device pixel ratio, hit target sizes, and virtual controls.
- Use Playwright mobile emulation and, when available, a real device smoke test.

---

# Performance Pass Prompt

Use `threejs-debug-profiler` to profile and improve this game.

Target device or budget:
- FPS/frame-time target:
- Target viewport:
- Known bottleneck:

Measure:
- frame time
- renderer draw calls
- geometry/material/texture counts
- texture dimensions and formats
- shader/post-processing cost
- bundle size
- memory/disposal behavior during state transitions

Implement only changes justified by measurements, then rerun the same checks.

---

# Scene Debug Prompt

Use `threejs-debug-profiler` to diagnose this Three.js issue:

Symptom:
- Blank canvas, black frame, frozen frame, bad resize, asset failure, bad animation timing, collision issue, or mobile input problem:

Recent changes:
- 

Expected behavior:
- 

Debugging requirements:
- Reproduce locally.
- Inspect console and page errors.
- Check canvas size, renderer size, camera aspect, near/far planes, lights, material visibility, asset paths, async loading, and animation loop ownership.
- Make the smallest fix that addresses the root cause.
- Verify with a browser screenshot and canvas-pixel sample.
