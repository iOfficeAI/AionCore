# Game Design And Level Design

Use this reference before broad new-game creation, major mechanic changes, progression tuning, level/arena/track/wave design, combat encounters, or any claim that gameplay is premium, polished, complete, or less generic.

This is not a long design document. It is a compact player-facing contract that turns an idea into implementable rules, spaces, pacing, and tuning checks.

Research basis: Unity's GDD guidance emphasizes player goals, rules/mechanics, difficulty, core loop, and feedback; MDA separates mechanics, runtime dynamics, and player experience; Unity's level-design material emphasizes concept, blockout/greybox, playtest, and iteration; Steve Swink's game-feel framing centers real-time control, simulated space, and polish.

## Design Brief Gate

Before implementation, write a brief with:

- Player promise: the fantasy in one sentence.
- Experience intent: one primary emotion, two supporting emotions, and anti-goal emotions to avoid (input lag frustration, unclear-objective confusion, constant-peak numbness).
- Genre-fit emotion mapping: action may stress mastery and stimulation; racing, speed and risk; horror, anticipatory anxiety and release; puzzle, curiosity and insight; narrative, empathy and conflict; cozy, safety and attachment. Do not apply one explosion/shake/loud-music kit to every genre.
- Internal craft baselines (not user-facing copy): camera contract, control response, teaching cadence, feedback hierarchy, content density, narrative hooks, art language, and audio dynamics drawn from mature work in the genre. Do not name those titles to the user or copy protected assets.
- Target feeling: tense, fast, tactical, elegant, chaotic, precise, cozy, etc.
- Primary verb: steer, aim, shoot, place, dodge, bank, sink, parry, solve.
- Secondary verbs: boost, upgrade, collect, block, drift, aim, reload, switch lane.
- Core loop: what the player repeats every 5-30 seconds.
- Progression loop: what changes across 1-5 minutes.
- Emotion beat sheet for the 3-5 chapter short game. Each beat names: target emotion, player action, trigger, risk/cost, rule or space change, camera/light/VFX, audio state, reward, and estimated duration. Emotion shifts must be carried by playable events, not title cards alone.
- Default arc to adapt: setup of expectation → agency → rising pressure → contrast/release → new variable or reversal → mastery test → climax → aftertaste and payoff. Intensity, order, and duration follow the genre; one intensity for the whole session is a failure.
- Intrinsic retention: agency, competence, curiosity, anticipated reward, growth, and emotional investment. Peak-end: the last minutes hold the strongest intended peak and a settle screen with continue, retry, and share. No dark patterns (forced waits, fake scarcity, nag overlays).
- First-session timing: identity/place/next action within 5 seconds; first valid input within 30 seconds; first perceptible reward within 60 seconds; core loop and short-term goal in 3-5 minutes.
- Fail/retry loop: how the player loses, learns, and restarts.
- Scoring/economy: what is rewarded, what is spent, what creates risk.
- Skill expression: what a better player does differently.
- Readability promise: how the next decision is communicated.
- Non-goals: features intentionally out of scope for this slice.

Reject "explore a cool scene" as a design brief. A game brief must include player decisions, pressure, feedback, consequence, and a named emotion arc. These ledgers are production tools; do not recite them to the user as a pitch.

## Core Loop Contract

Write the loop as:

```text
Player does [verb] to achieve [objective] while [pressure] creates risk; success gives [reward/progression], failure causes [cost/retry].
```

Then prove the loop in code:

- The primary verb is mapped to real input.
- The objective is visible in the world or HUD.
- Pressure exists within the first playable minute.
- Reward/progression changes game state, not only visuals.
- Failure or setback teaches what happened.
- Restart/retry is fast enough to encourage another attempt.

## MDA Check

Use this as a compact design review:

- Mechanics: exact rules, controls, collisions, timers, spawn tables, physics, scoring.
- Dynamics: what happens when a real player uses those rules under pressure.
- Aesthetics: the intended feeling and whether screenshots/playtest evidence supports it.

If the intended feeling does not emerge from the current mechanics, change mechanics or level layout. Do not try to fix missing dynamics with graphics alone.

## Level And Encounter Plan

Before building a track, arena, map, wave set, table, or puzzle space, define:

- Spatial format: lane, circuit, arena, corridor, open field, table, grid, tower path, puzzle room.
- Camera contract: what the camera can and cannot see.
- Player start, first decision, first reward, first threat.
- Safe zone or learning space, if the genre needs one.
- Main route plus optional risk/reward route, if applicable.
- Landmarks or orientation anchors.
- Escalation: how challenge increases every 20-60 seconds or per wave/hole/lap/phase.
- Recovery beats: where the player can breathe or regain control.
- Failure readability: how hazards, attacks, misses, and penalties are telegraphed.
- Reuse plan: which pieces are modular, randomized, or parameterized.

Greybox first: use simple shapes to prove scale, route, timing, line-of-sight, collision, and pacing before investing in art detail.

## Content Arc

A complete delivery (unless the user explicitly asked for a prototype) provides 3-5 named chapters, levels, or encounters that form a short game the player can finish from opening to settle. Each segment must change at least one of: objective, rules, risk, spatial relationship, or emotional state. "The player could theoretically keep repeating" is not a complete short game. Endless modes count only when they evolve through named stages inside that arc.

Narrative games use setup → escalation → payoff with at least two reversals that change the player's understanding, goal, or stance. Non-narrative games still need a readable dynamic arc through difficulty, ability, space, and music state. Failures must be understandable and inform the next decision. Recovery beats manage cognitive load and amplify the next pressure; they are not empty padding.

Greybox the full emotion curve before art-finishing the hero and two or three spaces that carry the key turns. Reuse systems; change layout, combination, risk, and context.

## Genre Patterns

### Endless Runner

- Teach lanes or steering with an early safe segment.
- Alternate compression and release: dense hazard groups, then reward/visibility windows.
- Use at least three obstacle families with distinct silhouettes and telegraphs.
- Difficulty can ramp via speed, lane pressure, obstacle combinations, and reward placement.

### Arcade Racer

- Define handling fantasy first: drift-heavy, grip racing, hover glide, combat racer, rally.
- Track should have readable apexes, braking/drift cues, recovery width, landmarks, and route rhythm.
- Add skill tests: clean racing line, boost timing, drift angle, traffic threading, shortcut risk.

### Dogfight / Space Shooter

- Define engagement range, turn rate, projectile speed, lock-on/lead affordance, and escape options.
- Encounters need target readability, off-screen threat indicators, and moments to reacquire orientation.
- Use waves or objectives that force movement, not only circular chasing.

### Tower Defense

- Define path topology, chokepoints, build zones, enemy archetypes, tower roles, economy cadence, and wave tells.
- Good maps create placement decisions, not just optimal obvious tiles.
- Waves should test different tower roles and expose upcoming enemy types before punishment.

### Billiards / Pool / Snooker

- Physics and rules are the game design. Use readable shot aim, cue force, spin/english, turn state, legal target feedback, foul feedback, and camera reset.
- Level design is table readability: pockets, rails, ball colors, aim lines, shadows, and overhead/low camera options.

### Mini Golf

- Each hole should have one clear read, one trick, and one risk/reward route.
- Escalate via ramps, banks, moving blockers, portals, split paths, gravity changes, and timing windows.
- Avoid holes where the first shot outcome is unreadable from the tee.

### Boss Fight / Action Arena

- Define boss phases, telegraphs, recovery windows, player punish windows, arena hazards, and camera lock behavior.
- Every attack needs a readable tell, avoid/defend option, impact feedback, and cooldown.
- Phases should add combinations or arena pressure, not just more health.

### Puzzle / Physics Game

- State the rule being taught in each puzzle.
- First puzzle teaches, second confirms, third twists.
- Failure should reveal information. Avoid hidden dependency chains that require guessing.

## Difficulty And Pacing

Use a curve, not random escalation:

- Introduce one new concept at a time.
- Combine known concepts after they are understood.
- Add breathing space after high-pressure moments.
- Increase challenge through timing, density, speed, resource scarcity, enemy mix, or spatial constraints.
- Keep early failures recoverable unless the genre is intentionally harsh.
- Tune with named constants and record changes.

## Fun-Factor Rejection Tests

Reject or iterate if any are true:

- The first 30 seconds lack a real decision.
- Identity, place, or next action is unclear after 5 seconds of a clean load.
- The player can ignore the main mechanic and still progress.
- The objective is unclear without reading source code or instructions.
- Failure happens before the player can understand why.
- Challenge is only "more things" rather than better combinations.
- Rewards do not change strategy, score, progression, or feel.
- The space is decorative and does not shape decisions.
- The game is fun only in the designer's explanation, not in active play.
- One intensity, one music loop, or one layout is held for the whole session.
- Emotion shifts exist only in text or cutscenes, not in playable events.
- Content duration is claimed from infinite grind rather than a named chapter ledger.

## Report Requirements

Report:

- Game design brief, including experience intent and anti-goal emotions.
- Core loop contract.
- Level/encounter plan.
- Emotion beat sheet and chapter content ledger (or an explicit prototype exception).
- Difficulty/pacing plan.
- Tuning constants changed.
- Fun-factor rejection tests passed or remaining failures.
- Fresh-eyes or adversarial note: expected emotion → observed behavior/feedback → delta → fix.
- Evidence from active play, not just a screenshot. Do not claim user retention or felt emotion as measured fact without a real player test.
