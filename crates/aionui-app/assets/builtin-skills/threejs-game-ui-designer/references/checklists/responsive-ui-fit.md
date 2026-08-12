# Responsive UI Fit Checklist

- Desktop, laptop, narrow, and mobile viewports have been checked.
- Safe-area padding is applied for mobile edges and notches when controls sit near edges.
- No text clips, overflows, overlaps, or becomes unreadably small.
- Long labels wrap or compress intentionally without breaking button height.
- Controls keep stable hit targets and do not shift during score/time/state changes.
- Layout uses grid/flex/container-aware constraints instead of viewport-only guesses where practical.
- Menu content remains reachable without hidden offscreen controls.
- Touch controls are large enough, separated enough, and do not trigger page scroll.
- Canvas resizing does not desynchronize HUD placement from gameplay framing.
- Screenshots or Playwright artifacts prove fit at the tested sizes.
