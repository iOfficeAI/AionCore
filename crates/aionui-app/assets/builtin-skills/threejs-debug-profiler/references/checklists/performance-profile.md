# Performance Profile Checklist

- Record target device, browser, viewport, DPR, and build mode.
- Measure FPS and frame time after warmup.
- Capture `renderer.info.render.calls`, triangles, points, lines, geometries, and textures.
- Count visible objects and animated objects.
- Identify expensive materials, custom shaders, shadows, reflections, and post-processing passes.
- Check texture dimensions, compression, mipmaps, anisotropy, and unused assets.
- Check geometry reuse, instancing opportunities, and object churn in the update loop.
- Check garbage collection pressure from per-frame allocations.
- Check resource disposal during scene changes.
- Compare bundle size before and after major dependencies.
- Re-measure after each optimization.
