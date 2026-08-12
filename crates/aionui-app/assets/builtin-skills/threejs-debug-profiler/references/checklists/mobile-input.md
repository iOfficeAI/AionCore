# Mobile Input Checklist

- Include `<meta name="viewport" content="width=device-width, initial-scale=1.0">`.
- Set canvas and touch overlays to `touch-action: none` only where needed.
- Use Pointer Events when possible.
- Keep controls reachable with thumbs and away from safe-area edges.
- Avoid tiny text or buttons below practical touch size.
- Verify portrait and landscape if both are supported.
- Check high-DPR canvas scaling and cap DPR if performance suffers.
- Prevent page scroll/zoom gestures from stealing gameplay input.
- Test virtual joystick/button release paths when the pointer leaves the control.
- Verify with Playwright mobile emulation and real hardware when available.
