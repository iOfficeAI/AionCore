---
name: threejs-debug-profiler
description: "Debug and profile Three.js browser games. Combines scene debugging, render/runtime/loading/animation/resize/mobile input fixes, performance profiling, draw calls, triangles, textures, memory, shader/post-processing cost, bundle size, and mobile DPR/input issues."
---

# Three.js Debug Profiler

## Purpose

Find root causes and optimize measured bottlenecks without breaking playability.

## Debug Workflow

Load `references/debug-profile-checklists.md` as the first action when debugging render/runtime/mobile issues, asset loading, audio loading/playback, animation, resize, input, blank canvas, physics/collision bugs, or profiling performance. Track it in a reference ledger with yes/no, path, and failure reason. Do not mark the debug/profile phase complete while this reference is skipped for debug or profiling work.

Load `references/checklists/scene-debugging.md` for render/runtime bug diagnosis, `references/checklists/performance-profile.md` for profiling work, and `references/checklists/mobile-input.md` for mobile render/input issues. Load `references/prompt-templates.md` only when the user asks for reusable debug/profile prompts or a task template.

1. Reproduce locally.
2. Read console/page/network errors.
3. Check canvas display size and drawing-buffer size.
4. Check renderer/context/loop ownership.
5. Check camera, aspect, near/far, lights, materials, fog, scene contents, transforms.
6. Check asset paths/loaders/CORS/base path.
7. Check animation delta units, physics/update order, fixed timestep, collider/body ownership, input listeners, pointer/touch behavior, resize, and audio context unlock/decode errors when audio is involved.
8. Fix root cause in owning module.
9. Verify browser screenshot, nonblank canvas, console/page errors, and broken path.

## Performance Workflow

1. Reproduce in correct build mode.
2. Record baseline: FPS/frame time, draw calls, triangles, geometries, textures, memory, bundle.
3. Identify CPU/GPU/memory/network bottleneck.
4. Optimize one thing at a time: instancing, shared resources, culling, LOD, DPR cap, cheaper shadows/post, texture discipline.
5. Re-measure same scenario and verify visuals/playability.

## Final Response

Lead with root cause or bottleneck. Report the reference ledger, checklist items used, files changed, baseline/post metrics, commands, screenshots/artifacts, broken paths retested, and residual risks.
