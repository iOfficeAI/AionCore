type MusicState = 'explore' | 'pressure' | 'settle';

type GameProgress = {
  score: number;
  target: number;
  complete: boolean;
  chapter?: string | number;
  failed?: boolean;
  hit?: boolean;
  hits?: number;
  paused?: boolean;
  dashing?: boolean;
  player?: { speed?: number };
};

function musicStateFromProgress(progress: GameProgress): MusicState {
  if (progress.complete) return 'settle';
  if (progress.failed) return 'pressure';
  const chapter = String(progress.chapter ?? '').trim().toLowerCase();
  if (chapter === 'explore' || chapter === 'pressure' || chapter === 'settle') return chapter;
  if (chapter === '1' || chapter === 'intro') return 'explore';
  if (chapter === '2' || chapter === 'mid') return 'pressure';
  if (chapter === '3' || chapter === 'end') return 'settle';
  if (progress.target > 0 && progress.score >= progress.target * 0.5) return 'pressure';
  return 'explore';
}

function eventsFromDiagnostics(
  prev: GameProgress | null,
  next: GameProgress | null,
): Array<{ id: string }> {
  const events: Array<{ id: string }> = [];
  if (!next) return events;
  const before = prev || { score: 0, target: 0, complete: false };
  if ((next.score || 0) > (before.score || 0)) events.push({ id: 'pickup' });
  if (next.complete && !before.complete) {
    events.push({ id: 'win' });
    events.push({ id: 'voice:settle' });
  }
  if (next.failed && !before.failed) events.push({ id: 'fail' });
  const hits = next.hits || (next.hit ? 1 : 0);
  const prevHits = before.hits || (before.hit ? 1 : 0);
  if (hits > prevHits) events.push({ id: 'hit' });
  if (next.paused && !before.paused) events.push({ id: 'pause' });
  if (next.dashing && !before.dashing) events.push({ id: 'dash' });
  const speed = next.player?.speed || 0;
  const prevSpeed = before.player?.speed || 0;
  if (!next.dashing && speed >= 7.2 && prevSpeed < 7.2) events.push({ id: 'dash' });
  return events;
}

type KitFile = { file: string; group?: string; loop?: boolean };
type VoiceLine = { id: string; file: string; cue?: string; text?: string };

type AudioKit = {
  music?: {
    file: string;
    states: Record<string, { start: number; duration: number }>;
  };
  sfx?: Record<string, KitFile>;
  voice?: { musicVocals?: boolean; tts?: boolean; lines?: VoiceLine[] };
};

const GROUPS = ['master', 'music', 'ambience', 'sfx', 'ui', 'voice'] as const;

export class AudioSystem {
  private context: AudioContext | null = null;
  private unlocked = false;
  private kit: AudioKit | null = null;
  private readonly buffers = new Map<string, AudioBuffer>();
  private readonly gains = new Map<string, GainNode>();
  private musicSource: AudioBufferSourceNode | null = null;
  private ambienceSource: AudioBufferSourceNode | null = null;
  private musicState: MusicState = 'explore';
  private musicArmed = false;
  private raf = 0;
  private lastPickupAt = 0;
  private lastEventAt = new Map<string, number>();
  private prevProgress: GameProgress | null = null;
  private inputUnbind: (() => void) | null = null;
  private introPlayed = false;

  constructor() {
    const unlock = () => {
      void this.unlock();
      window.removeEventListener('pointerdown', unlock);
      window.removeEventListener('keydown', unlock);
    };
    window.addEventListener('pointerdown', unlock, { once: true });
    window.addEventListener('keydown', unlock, { once: true });
    (window as unknown as { __AION_AUDIO__?: AudioSystem }).__AION_AUDIO__ = this;
  }

  async unlock(): Promise<void> {
    if (this.unlocked) return;
    const AudioContextClass =
      window.AudioContext ||
      (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!AudioContextClass) return;
    this.context = new AudioContextClass();
    await this.context.resume();
    this.installGraph();
    this.unlocked = true;
    await this.loadKit();
    this.startAmbience();
    this.setMusic('explore');
    this.bindPlaybackInput();
    this.playCue('intro');
    this.watchProgress();
  }

  pickup(index: number): void {
    if (!this.context || this.context.state !== 'running') return;
    const now = this.context.currentTime;
    if (now - this.lastPickupAt < 0.35) return;
    this.lastPickupAt = now;
    this.duckMusic(0.12);
    if (this.buffers.has('pickup')) {
      this.playBuffer('pickup', 'sfx', 0.9, 0.94 + (index % 5) * 0.03);
      return;
    }
    this.playTone(320 + index * 22, 680 + index * 24, 0.18, 'triangle', 0.08);
  }

  dash(): void {
    this.playOneShot('dash', 'sfx', 0.35);
  }

  hit(): void {
    this.playOneShot('hit', 'sfx', 0.2);
  }

  fail(): void {
    this.playOneShot('fail', 'sfx', 0.4);
  }

  win(): void {
    this.playOneShot('win', 'ui', 1);
  }

  pause(): void {
    this.playOneShot('pause', 'ui', 0.3);
  }

  confirm(): void {
    this.playOneShot('confirm', 'ui', 0.2);
  }

  say(id: string): void {
    this.playVoice(id);
  }

  setMusic(state: MusicState): void {
    if (this.musicState === state && this.musicArmed) return;
    this.musicState = state;
    if (!this.context || this.context.state !== 'running') return;
    this.stopSource(this.musicSource);
    this.musicSource = null;
    this.musicArmed = true;
    const region = this.kit?.music?.states[state];
    const score = this.buffers.get('score');
    if (score && region) {
      this.musicSource = this.loopRegion(score, 'music', region.start, region.duration, 0.22);
      return;
    }
    this.playProceduralBed(state);
  }

  sync(progress: GameProgress): void {
    this.setMusic(musicStateFromProgress(progress));
  }

  dispose(): void {
    cancelAnimationFrame(this.raf);
    this.inputUnbind?.();
    this.inputUnbind = null;
    this.stopSource(this.musicSource);
    this.stopSource(this.ambienceSource);
    void this.context?.close();
    this.context = null;
    this.buffers.clear();
    this.gains.clear();
  }

  private installGraph(): void {
    if (!this.context) return;
    const master = this.context.createGain();
    master.gain.value = 1;
    master.connect(this.context.destination);
    this.gains.set('master', master);
    for (const name of GROUPS) {
      if (name === 'master') continue;
      const gain = this.context.createGain();
      gain.gain.value = name === 'music' ? 0.22 : name === 'ambience' ? 0.16 : name === 'voice' ? 0.9 : 0.8;
      gain.connect(master);
      this.gains.set(name, gain);
    }
  }

  private async loadKit(): Promise<void> {
    try {
      const response = await fetch('/audio/kit.json');
      if (!response.ok) return;
      this.kit = (await response.json()) as AudioKit;
      if (this.kit.music?.file) {
        await this.decode('score', this.publicUrl(this.kit.music.file));
      }
      for (const [id, entry] of Object.entries(this.kit.sfx || {})) {
        await this.decode(id, this.publicUrl(entry.file));
      }
      for (const line of this.kit.voice?.lines || []) {
        await this.decode(line.id, this.publicUrl(line.file));
      }
    } catch {
      this.kit = null;
    }
  }

  private publicUrl(file: string): string {
    return file.startsWith('/') ? file : `/${file}`;
  }

  private async decode(id: string, url: string): Promise<void> {
    if (!this.context) return;
    const response = await fetch(url);
    if (!response.ok) return;
    const buffer = await this.context.decodeAudioData(await response.arrayBuffer());
    this.buffers.set(id, buffer);
  }

  private startAmbience(): void {
    const buffer = this.buffers.get('ambience');
    if (!buffer) return;
    this.ambienceSource = this.loopRegion(buffer, 'ambience', 0, buffer.duration, 0.16);
  }

  private bindPlaybackInput(): void {
    const onKey = (event: KeyboardEvent) => {
      if (event.repeat) return;
      if (event.code === 'Space' || event.code === 'ShiftLeft' || event.code === 'ShiftRight') {
        this.dash();
      }
      if (event.code === 'Escape' || event.code === 'KeyP') {
        this.pause();
      }
    };
    window.addEventListener('keydown', onKey);
    const dashButton = document.querySelector('#dash-button');
    const onDash = () => this.dash();
    dashButton?.addEventListener('pointerdown', onDash);
    this.inputUnbind = () => {
      window.removeEventListener('keydown', onKey);
      dashButton?.removeEventListener('pointerdown', onDash);
    };
  }

  private watchProgress(): void {
    const tick = () => {
      const diagnostics = window.__THREE_GAME_DIAGNOSTICS__;
      if (diagnostics) {
        const progress: GameProgress = {
          score: diagnostics.score,
          target: diagnostics.targetScore,
          complete: diagnostics.complete,
          chapter: (diagnostics as GameProgress).chapter,
          failed: (diagnostics as GameProgress).failed,
          hit: (diagnostics as GameProgress).hit,
          hits: (diagnostics as GameProgress).hits,
          paused: (diagnostics as GameProgress).paused,
          dashing: (diagnostics as GameProgress).dashing,
          player: diagnostics.player,
        };
        this.sync(progress);
        for (const event of eventsFromDiagnostics(this.prevProgress, progress)) {
          this.applyEvent(event.id, progress.score);
        }
        this.prevProgress = progress;
      }
      this.raf = requestAnimationFrame(tick);
    };
    this.raf = requestAnimationFrame(tick);
  }

  private applyEvent(id: string, score: number): void {
    if (id === 'pickup') {
      this.pickup(score);
      return;
    }
    if (id === 'voice:settle') {
      this.playCue('settle');
      return;
    }
    if (id === 'dash') {
      this.dash();
      return;
    }
    if (id === 'hit') {
      this.hit();
      return;
    }
    if (id === 'fail') {
      this.fail();
      return;
    }
    if (id === 'win') {
      this.win();
      return;
    }
    if (id === 'pause') {
      this.pause();
    }
  }

  private playCue(cue: string): void {
    if (cue === 'intro' && this.introPlayed) return;
    const line = this.kit?.voice?.lines?.find((item) => item.cue === cue);
    if (!line) return;
    if (cue === 'intro') this.introPlayed = true;
    this.playVoice(line.id);
  }

  private playVoice(id: string): void {
    if (!this.buffers.has(id)) return;
    this.duckMusic(0.45);
    this.playBuffer(id, 'voice', 0.95, 1);
  }

  private playOneShot(id: string, fallbackGroup: string, cooldown: number): void {
    if (!this.context || this.context.state !== 'running') return;
    const now = this.context.currentTime;
    if (now - (this.lastEventAt.get(id) || 0) < cooldown) return;
    this.lastEventAt.set(id, now);
    const group = this.kit?.sfx?.[id]?.group || fallbackGroup;
    if (this.buffers.has(id)) {
      this.playBuffer(id, group, 0.86, 1);
      return;
    }
    if (id === 'dash') this.playTone(180, 90, 0.16, 'sine', 0.06);
    else if (id === 'hit') this.playTone(140, 70, 0.14, 'square', 0.07);
    else if (id === 'fail') this.playTone(220, 90, 0.28, 'sawtooth', 0.06);
    else if (id === 'win') this.playTone(440, 660, 0.32, 'triangle', 0.07);
    else this.playTone(300, 220, 0.1, 'sine', 0.05);
  }

  private playBuffer(id: string, group: string, volume: number, rate: number): void {
    if (!this.context) return;
    const buffer = this.buffers.get(id);
    if (!buffer) return;
    const source = this.context.createBufferSource();
    const gain = this.context.createGain();
    source.buffer = buffer;
    source.playbackRate.value = rate;
    gain.gain.value = volume;
    source.connect(gain).connect(this.gains.get(group) ?? this.context.destination);
    source.start();
  }

  private loopRegion(
    buffer: AudioBuffer,
    group: string,
    start: number,
    duration: number,
    volume: number,
  ): AudioBufferSourceNode | null {
    if (!this.context) return null;
    const source = this.context.createBufferSource();
    const gain = this.context.createGain();
    const end = Math.min(buffer.duration, Math.max(start + 0.25, start + duration));
    source.buffer = buffer;
    source.loop = true;
    source.loopStart = Math.max(0, start);
    source.loopEnd = end;
    gain.gain.value = 0.0001;
    source.connect(gain).connect(this.gains.get(group) ?? this.context.destination);
    source.start(0, source.loopStart);
    gain.gain.exponentialRampToValueAtTime(volume, this.context.currentTime + 0.18);
    return source;
  }

  private playProceduralBed(state: MusicState): AudioBufferSourceNode | null {
    if (!this.context) return null;
    const freqs = state === 'pressure' ? [196, 247, 294] : state === 'settle' ? [174, 220] : [196, 247];
    const now = this.context.currentTime;
    const master = this.gains.get('music') ?? this.context.destination;
    for (const freq of freqs) {
      const oscillator = this.context.createOscillator();
      const gain = this.context.createGain();
      oscillator.type = 'sine';
      oscillator.frequency.value = freq;
      gain.gain.value = 0.03;
      oscillator.connect(gain).connect(master);
      oscillator.start(now);
      oscillator.stop(now + 8);
    }
    return null;
  }

  private playTone(
    from: number,
    to: number,
    duration: number,
    type: OscillatorType,
    volume: number,
  ): void {
    if (!this.context) return;
    const oscillator = this.context.createOscillator();
    const gain = this.context.createGain();
    const now = this.context.currentTime;
    oscillator.type = type;
    oscillator.frequency.setValueAtTime(from, now);
    oscillator.frequency.exponentialRampToValueAtTime(Math.max(40, to), now + duration * 0.7);
    gain.gain.setValueAtTime(0.0001, now);
    gain.gain.exponentialRampToValueAtTime(volume, now + 0.02);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + duration);
    oscillator.connect(gain).connect(this.gains.get('sfx') ?? this.context.destination);
    oscillator.start(now);
    oscillator.stop(now + duration + 0.02);
  }

  private duckMusic(seconds: number): void {
    const music = this.gains.get('music');
    if (!music || !this.context) return;
    const now = this.context.currentTime;
    music.gain.cancelScheduledValues(now);
    music.gain.setValueAtTime(music.gain.value, now);
    music.gain.linearRampToValueAtTime(0.08, now + 0.03);
    music.gain.linearRampToValueAtTime(0.22, now + seconds);
  }

  private stopSource(source: AudioBufferSourceNode | null): void {
    try {
      source?.stop();
    } catch {
      // already stopped
    }
    source?.disconnect();
  }
}
