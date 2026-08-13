# New Game Definition Of Done

Prototype exception: apply only the install/build/first-input items plus a labeled short slice when the user explicitly asked for a prototype. Otherwise all items below are required.

- The project installs with `npm install`.
- `npm run dev` starts a local Vite server.
- `npm run play` is the package script; **launch with** `node <gameplay-skill>/scripts/launch_game.mjs <game-dir>` so Vite is detached. Do not foreground `npm run play` in ExecCommand.
- `npm run build` completes.
- The first screen is the game, not a landing page.
- Within 5 seconds the player can tell who they are, where they are, and what to do next.
- The player can interact within 30 seconds; the first perceptible reward lands within 60 seconds.
- A compact game design brief exists: player promise, experience intent (primary/supporting/anti-goal emotions), primary verb, objective, pressure, reward, fail/retry, and non-goals.
- An emotion beat sheet exists for the 3-5 chapter short game, with playable events carrying each shift.
- A chapter/level/encounter ledger names 3-5 segments; each changes objective, rules, risk, space, or emotional state. Infinite repetition does not count as a complete short game.
- A level/encounter plan exists for the first playable space, track, arena, wave, hole, table, or puzzle.
- There is a clear objective, score, timer, health, level target, or fail condition.
- The core loop is proven through real input, not only described in text.
- Narrative games have setup, escalation, payoff, and at least two reversals that change understanding, goal, or stance. Non-narrative games have a readable dynamic arc through difficulty, ability, space, and music state.
- The hero/player is not a capsule, cube, or default primitive plus glow in the final delivery.
- Audio has a narrative matrix (ambience, interaction, threat, music states). Music is not one loop for the whole session unless the genre's intent is a single restrained bed with documented contrast.
- Pause and settle screens expose share (Web Share with clipboard fallback). `localhost` is reported as a local playtest URL only.
- Keyboard/mouse input works on desktop.
- Touch or pointer input works if mobile is in scope.
- The camera frames the playable area at desktop and mobile sizes.
- HUD text is readable and does not cover critical gameplay.
- Browser console has no blocking errors.
- A screenshot proves the canvas rendered.
- A canvas-pixel check proves the canvas is not blank.
- A fresh-eyes or adversarial pass records expected emotion → observed behavior/feedback → delta → fix. Do not claim user-measured retention without a real player test.
- The game was actually launched for the user. The user-facing close names how to open, controls, current goal, and share — not a command as the first sentence.
- Final report names design brief, emotion beat sheet, level/encounter plan, controls, verification evidence, share status, and remaining risks.
