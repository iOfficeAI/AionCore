# Debug And Profile Checklists

Use this for blank canvases, bad framing, runtime errors, asset/audio loading issues, animation/collision/input failures, mobile bugs, and performance optimization.

## Triage Order

1. Reproduce locally with the same command and URL the user used when possible.
2. Capture console, page, and network errors.
3. Confirm the app is serving the expected build, not another local server on the same port.
4. Identify the owner: renderer, scene, camera, loop, assets, audio, input, physics, UI, CSS, build/base path, or performance.
5. Fix the root cause in the owning module.
6. Retest the exact broken path.

## Blank Or Bad Canvas

Check in this order:

- Canvas exists in the DOM.
- Canvas CSS size is nonzero and visible.
- Drawing buffer size is nonzero and matches expected DPR behavior.
- WebGL context creation succeeded.
- Renderer is rendering inside exactly one active loop.
- Camera has correct aspect, projection matrix, near/far, and points at visible content.
- Scene has visible objects at expected transforms and scale.
- Materials are visible: opacity, transparent, side, depth, color space, fog interaction.
- Lights exist when lit materials need them.
- Background/fog is not matching object color.
- Resize updates renderer, camera, composer, and CSS.
- CSS overlays are not covering the canvas.
- Render target/composer output is displayed.

## Asset Loading

Check:

- URLs and Vite base path.
- Files in `public/` or imported asset paths.
- Loader type and `three/addons/...` imports.
- CORS and MIME type errors.
- glTF external buffers/textures.
- Texture color space and flipY where relevant.
- Async load state, loading UI, error fallback, and retry behavior.
- Disposal of replaced assets.

For generated/imported GLB assets, also check file size, URL casing, Vite public/import path, Draco/Meshopt requirements, scene scale, pivot/origin, bounds, texture dimensions, material count, animation clip names, and whether generated download URLs were saved before expiring.

## Audio Loading And Playback

Check:

- Audio files exist at runtime URLs and have compatible MIME types.
- `AudioContext` is resumed from a user gesture before playback.
- Decode/load promises reject visibly instead of failing silently.
- SFX triggers are event-driven and not firing every frame.
- Ambience/music loops stop on pause, restart, and scene teardown.
- Mute/volume state controls every group.
- Page visibility pause/resume does not stack duplicate sources.
- Mobile browser unlock behavior is tested when mobile is in scope.

## Animation, Loop, And Physics

Check:

- Delta time units in seconds vs milliseconds.
- Delta clamping for tab sleep and frame spikes.
- Fixed-step physics accumulator if physics is timing-sensitive.
- Physics engine initialized before creating bodies or stepping.
- Physics world owns body/collider creation and disposal.
- Physics timestep is stable and not tied directly to variable render delta.
- Animation mixer updates and clip actions.
- Multiple requestAnimationFrame loops.
- State transitions that stop updates or restart timers.
- Collision proxies vs visual meshes.
- Collider scale, rotation, and offset match the visual expectation.
- High-speed tunneling and spawn overlap.
- CCD enabled only for high-speed bodies that need it.
- Sensors/triggers have active events or explicit overlap checks.
- Kinematic moving platforms update physics bodies, not only visual meshes.
- Restart cleanup for entities, listeners, timers, effects, and physics bodies.
- Imported model animation mixer exists, clips are bound to the correct root, root motion is intentional, and clip actions are stopped/cleaned up on restart.

## Input And Mobile Bugs

Check:

- Keyboard focus and prevented default only where needed.
- Pointer listeners attached to the correct element.
- Pointer capture and release/cancel behavior.
- `touch-action` CSS and viewport meta.
- Page scroll stealing gestures.
- Device pixel ratio causing tiny controls or high GPU cost.
- Safe-area insets.
- Orientation/resize after rotation.
- Desktop input still works after mobile controls are added.
- UI controls emit game intents and do not directly duplicate simulation rules.

## Performance Profiling Order

Measure in production preview when user-facing performance matters.

1. Establish scenario: viewport, DPR, route, gameplay state, camera view, mobile/desktop.
2. Baseline:
   - FPS/frame time.
   - Renderer calls.
   - Triangles.
   - Geometries.
   - Materials.
   - Textures.
   - Render targets/post passes.
   - JS heap or memory estimate when available.
- Bundle and large assets when relevant.
   - Imported model file sizes, animation clips, and texture dimensions when generated 3D assets were added.
   - Physics body count, collider count, sensors, CCD-enabled bodies, active contacts/pairs, and physics step cost when physics changed.
3. Classify bottleneck:
   - CPU: simulation, allocations, pathfinding, physics, animation mixers, UI layout.
   - GPU draw: draw calls, material switches, too many unique meshes.
   - GPU fragment: overdraw, post-processing, high DPR, transparent particles.
   - GPU vertex: high triangle count, dense shadows.
   - Memory: textures, render targets, undisposed resources.
   - Network/bundle: large dependencies or assets.
4. Apply one optimization.
5. Re-measure the same scenario.
6. Check visual/playability regression.

## Preferred Optimizations

- InstancedMesh for repeated detail.
- Shared geometries/materials/textures.
- Object pools for effects, bullets, pickups, and debris.
- Frustum/distance culling.
- LOD for background props and repeated world kits.
- DPR cap or adaptive quality.
- Cheaper shadows: fewer casters, smaller maps, static/contact alternatives.
- Limited post-processing passes.
- Texture atlases, compression, reuse, and mipmaps.
- Avoid per-frame allocations and unnecessary layout reads.
- Reduce physics cost with simple colliders, sleeping, fewer dynamic bodies, collision groups, pooled bodies, and narrower sensors before removing important gameplay.
- Dispose geometries, materials, textures, render targets, and audio resources.
- Use `threejs-3d-generator` face limits, smart-low-poly, conversion, lower texture size/quality, or simpler imported variants before deleting important hero readability.

## Renderer Diagnostics Snippet

When possible, expose a diagnostic object:

```ts
window.__THREE_GAME_DIAGNOSTICS__ = {
  renderer: renderer.info,
  get state() {
    return game.getDebugState();
  },
};
```

Useful fields include `renderer.info.render.calls`, `triangles`, `points`, `lines`, `memory.geometries`, and `memory.textures`.

For physics-heavy games, add:

```ts
physics: {
  engine: 'rapier',
  timestep: 1 / 60,
  bodies: physicsWorld.bodyCount(),
  colliders: physicsWorld.colliderCount(),
  sensors,
  ccdBodies,
}
```

## Bug Report Format

```text
Issue:
Reproduction:
Expected:
Actual:
Root cause:
Fix:
Verification:
Residual risk:
```

## Common Mistakes

- Guessing without reproducing.
- Optimizing development-server performance instead of production preview.
- Removing visual detail before checking DPR, post, shadows, instancing, or culling.
- Fixing symptoms in CSS when renderer/camera sizing is wrong.
- Adding mobile controls without testing pointer cancel and safe areas.
- Ignoring console/page errors because the canvas appears nonblank.
- Shipping an imported model without checking scale, pivot, collision, animation clips, texture memory, or mobile cost.
