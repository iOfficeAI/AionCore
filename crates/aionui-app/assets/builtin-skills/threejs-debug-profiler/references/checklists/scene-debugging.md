# Scene Debugging Checklist

- Reproduce the issue locally before editing.
- Read the first console error and page error before changing code.
- Confirm the render loop starts once and continues.
- Confirm the renderer is attached to the expected canvas.
- Check canvas CSS size, drawing-buffer size, and device pixel ratio.
- Check camera position, target, aspect, projection update, near plane, and far plane.
- Check scene contains visible objects in front of the camera.
- Check lights, material side, opacity, depth settings, and color management.
- Check asset URLs, public paths, async load ordering, CORS, and failed network requests.
- Check animation mixer delta units and fixed-step simulation ordering.
- Check collision layers/radii/scale if gameplay appears broken.
- Verify the fix with screenshot, pixel sample, and interaction.
