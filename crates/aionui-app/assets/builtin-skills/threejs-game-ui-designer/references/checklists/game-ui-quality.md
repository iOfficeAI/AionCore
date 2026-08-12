# Game UI Quality Checklist

- The first screen is the playable game or a deliberate modal state, not a landing page.
- HUD hierarchy matches gameplay priority: survival/status, objective, feedback, secondary flavor.
- UI visual language matches the game's genre, world materials, color, and motion.
- Menus include expected states: pause, resume, restart, settings, win/lose when relevant.
- Buttons and controls have stable dimensions plus hover, pressed, focus, and disabled states.
- Icons are used for compact tool/actions when recognizable; ambiguous icons have labels or tooltips.
- UI does not use nested cards, marketing-page layout, or generic dashboard styling.
- UI does not block player, threats, goals, interactables, or near-future path unless intentionally modal.
- Dynamic values such as score, time, health, combo, and ammo do not shift layout.
- Reduced-motion and flash risk is considered for intense UI animation or damage feedback.
- UI state is driven by the game state model and does not duplicate simulation rules.
- Desktop and mobile screenshots show coherent composition and no clipped or overlapping controls.
