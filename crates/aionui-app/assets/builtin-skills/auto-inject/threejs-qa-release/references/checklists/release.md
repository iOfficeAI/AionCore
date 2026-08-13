# Release Checklist

- `npm run build` passes.
- `npm run preview` or equivalent static server runs the built files.
- Asset URLs work with the intended Vite `base` path.
- No local-only files, debug panels, or test overlays are visible unless intentionally gated.
- Console is clean in production preview.
- Desktop and mobile visual checks pass.
- Main interaction works in production preview.
- Bundle size and large assets are reviewed.
- License/source notes for third-party assets are present.
- Final report includes commands, screenshots/artifacts, known risks, and deployment notes.
