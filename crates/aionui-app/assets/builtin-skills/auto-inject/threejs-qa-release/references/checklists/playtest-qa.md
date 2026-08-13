# Playtest QA Checklist

- Start from a clean load.
- Confirm identity, place, and next action are readable within 5 seconds; first valid input within 30 seconds; first perceptible reward within 60 seconds.
- Walk the critical path through every named chapter, level, or encounter: each can be entered, its objective completed, and its emotion beat triggered. Do not substitute idle looping for this.
- Verify controls, camera, objective feedback, failure/retry, recovery beats, climax, and settle.
- Confirm audiovisual state changes on those beats (music/VFX/camera intensity matches the beat sheet).
- Try rapid input changes and edge movement against arena boundaries.
- Trigger collisions from multiple angles.
- Pause, restart, resize, and refocus the tab if supported.
- Check audio unlock and volume behavior after the first gesture.
- Open share on pause and settle: Web Share or clipboard fallback, success/failure feedback, and `?shared=1` entry. Label `localhost` as local playtest only.
- Watch for unreadable moments, camera occlusion, jitter, missed feedback, and sensory overload (peak held too long, next decision hidden).
- Capture screenshots for desktop and mobile.
- Record a fresh-eyes note: expected emotion → observed behavior/feedback → delta → fix. Do not claim user-measured retention without a real player test.
- Record bugs as reproduction steps with expected and actual behavior.
