# Three.js Audio Workflows

Use this reference before generating or integrating audio for a game.

## First Game Kit

For a new complete game, run the Node kit once instead of inventing a full matrix by hand:

```bash
node <this-skill-dir>/scripts/threejs_audio_asset.mjs probe
node <this-skill-dir>/scripts/threejs_audio_asset.mjs kit \
  --genre "<english genre>" --emotion "<english emotions>" \
  --verb "<experience-intent verb>" \
  --explore "<heard space, materials>" \
  --pressure "<escalation hearables>" \
  --settle "<aftertaste hearables>" \
  --spoken "no dialogue" \
  --voice auto --out <game-dir>
```

`--genre` plus `--scene` or `--explore/--pressure/--settle` are required. ElevenLabs `positive_styles` must be English; the kit maps common Chinese scene words, but English hearables produce a better score. Overlay plays generated SFX/TTS from `__THREE_GAME_DIAGNOSTICS__` (score, complete, speed, optional `chapter` / `failed` / `hits` / `paused` / `dashing`).

Voice follows the scene: singing/lyrics → music vocals; 旁白/对白/announcer → TTS; otherwise instrumental + SFX only. One score file, three loop regions (`explore` / `pressure` / `settle`). Overlay `AudioSystem` fetches `/audio/kit.json`. Expand the matrix below only for extra events the kit does not cover.

## Audio Planning

Create an audio matrix before generating files:

| Category | Required events | Asset count | Loop? | Runtime group |
| --- | --- | ---: | --- | --- |
| UI | hover, confirm, cancel, pause, fail | 3-8 | no | ui |
| Movement | jump, dash, boost, landing, drift | 3-10 | sometimes | sfx |
| Interaction | pickup, hit, shield, score, checkpoint | 4-12 | no | sfx |
| Threat | enemy attack, warning, impact, boss cue | 4-12 | no | sfx |
| Ambience | room tone, wind, engines, crowd, weather | 1-4 | yes | ambience |
| Voice | announcer, boss, tutorial, combat barks | optional | no | voice |
| Music | sting, base/explore, pressure, climax, settle — generate only the states the emotion curve needs | 2-5 | yes (beds) | music |

Sound narrative first: ambience defines space, interaction confirms cause, threat sets expectation, voice carries information, music controls emotional energy. High-frequency sounds need variants, cooldowns, and loudness ranks.

For a first complete pass, generate at least:

- 1 ambience loop.
- 3 UI sounds.
- 5 gameplay SFX tied to real events.
- Music states required by the emotion beat sheet (not a fixed four-track kit for every genre). A cozy or puzzle game may need a restrained bed plus a sting; an action game may need pressure and climax beds. One unchanged loop for the whole session is a fail unless the brief documents that single bed and its contrast.
- Optional voice only if the design benefits from dialogue or callouts.

## Prompting

Good prompts specify source, transient, tail, mix density, genre, and gameplay use:

```text
short [event] sound for [game genre], [material/source], clear transient, [tail length], no music, no voice, readable under gameplay mix
```

Examples:

- `short shield absorb impact for sci-fi boss fight, glassy plasma hit, bright transient, low sub thump, 0.8s tail, no music, no voice`
- `looping abandoned cathedral ambience for dark fantasy arena, distant wind through stone arches, subtle torch crackle, no melody, seamless loop`
- `tiny premium menu confirm click, soft mechanical latch, warm sparkle tail, no harsh beep`

Avoid prompts that are only mood words (`epic`, `AAA`, `cool`). Name the gameplay event.

## Generation Strategy

- Generate short SFX individually instead of one long mixed file.
- Make loops deliberately with `--loop`; test them looping in the game.
- Keep music out of SFX prompts. First games use `kit` (`POST /v1/music`, one score, three regions). Extra beds may use `music`. Do not fake a song with `--loop` SFX. Instrumental scenes must keep vocals out of the score.
- Keep UI sounds quieter and shorter than gameplay SFX.
- Generate variants for high-frequency events to avoid repetition.
- Normalize in the game through volume groups, not by editing every file manually during early iteration.
- Switch music on emotion/gameplay state with stings, layers, or crossfades; prefer phrase or beat boundaries. Duck music briefly under important SFX. Planned quiet is valid; unplanned silence, missing contact SFX, or constant peak loudness are not.

## Voice Strategy

Use TTS when the line can be generated from text and exact acting is less important.

Use voice-change when:

- The user or agent can record a scratch performance.
- Timing, breaths, laughter, anger, fear, or delivery need to be preserved.
- A boss, announcer, narrator, or character line needs stronger acting than text prompt alone.

Clean noisy scratch recordings with `isolate` before conversion unless `voice-change --remove-background-noise` is sufficient.

Do not generate or convert voices that imply impersonation of a real private person. For characters, describe a fictional voice style or use a licensed/available voice ID.

## Runtime Integration

Use a small audio manager instead of ad hoc `new Audio()` calls once a game has more than a few sounds:

- Load sounds after user gesture unlock.
- Maintain groups: `master`, `sfx`, `ui`, `ambience`, `voice`, `music`.
- Expose mute and per-group volume.
- Loop ambience and music beds through `AudioBufferSourceNode` or a library wrapper that handles loop restarts.
- Switch music beds on gameplay/emotion state with stings, layers, or crossfades; duck music under important SFX; prefer phrase boundaries.
- Stop/dispose old sources when restarting scenes.
- Do not trigger the same high-frequency SFX every frame; add cooldowns or variant pools.
- Pause/resume audio with the game pause state and page visibility.

Minimal Web Audio shape:

```ts
class GameAudio {
  private ctx = new AudioContext();
  private buffers = new Map<string, AudioBuffer>();
  private gains = new Map<string, GainNode>();

  async unlock() {
    if (this.ctx.state !== 'running') await this.ctx.resume();
  }

  async load(id: string, url: string) {
    const data = await fetch(url).then(r => r.arrayBuffer());
    this.buffers.set(id, await this.ctx.decodeAudioData(data));
  }

  play(id: string, group = 'sfx', volume = 1) {
    const buffer = this.buffers.get(id);
    if (!buffer) return;
    const source = this.ctx.createBufferSource();
    const gain = this.ctx.createGain();
    gain.gain.value = volume;
    source.buffer = buffer;
    source.connect(gain).connect(this.gains.get(group) ?? this.ctx.destination);
    source.start();
  }
}
```

## Verification Checklist

Report pass/fail:

- Files exist under `assets/audio/...`.
- Main gameplay events trigger the expected sounds.
- Ambience loop starts, loops, and stops cleanly.
- Pause/restart does not stack duplicate loops.
- Browser autoplay/user gesture unlock is handled.
- Volume/mute controls affect groups.
- Mobile Safari/Chrome are considered when audio context unlock is needed.
- Console has no decode/load errors.

## Final Evidence

Include:

- Audio matrix with generated file paths.
- Prompt/text/input/source for every generated or processed file.
- Duration, loop flag, output format, voice ID when applicable.
- Runtime trigger mapping.
- Verification notes and remaining gaps.
