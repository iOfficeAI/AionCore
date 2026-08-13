import * as THREE from 'three';
import { InputController } from '../core/InputController';
import { Loop } from '../core/Loop';
import { createRenderer, resizeRenderer } from '../core/Renderer';
import { Hazard } from '../systems/Hazard';
import { Pickup } from '../entities/Pickup';
import { Player, type ArenaBounds } from '../entities/Player';
import { AudioSystem } from '../systems/AudioSystem';
import { CameraRig } from '../systems/CameraRig';
import { CollisionSystem } from '../systems/CollisionSystem';
import { DebugTools, type DebugTuning } from '../systems/DebugTools';
import { Hud } from '../systems/Hud';
import { LookSystem } from '../systems/LookSystem';
import { PauseShare } from '../systems/PauseShare';
import { createWorldKit, supportY, type WorldKit } from '../systems/WorldKit';
import {
  ARENA as SESSION_ARENA,
  chapterFromScore,
  defaultSession,
  pickupLayout,
  type Chapter,
  type Session,
} from '../studio/session';
import { createSeededRandom } from '../utils/random';

const ARENA: ArenaBounds = {
  halfWidth: SESSION_ARENA.halfWidth,
  halfDepth: SESSION_ARENA.halfDepth,
};

export class Game {
  private readonly renderer: THREE.WebGLRenderer;
  private readonly scene = new THREE.Scene();
  private readonly camera = new THREE.PerspectiveCamera(48, 1, 0.1, 80);
  private readonly input: InputController;
  private readonly player = new Player();
  private readonly pickups: Pickup[] = [];
  private readonly collision = new CollisionSystem();
  private readonly audio = new AudioSystem();
  private readonly hud = new Hud();
  private readonly look = new LookSystem();
  private readonly cameraRig = new CameraRig(this.camera);
  private readonly loop = new Loop(
    (delta, elapsed) => this.update(delta, elapsed),
    () => this.render(),
  );

  private readonly tuning: DebugTuning & { gravity: number; jumpSpeed: number } = {
    speed: 5.8,
    dashMultiplier: 1.75,
    acceleration: 13,
    cameraLag: 0.16,
    exposure: 1.05,
    maxDpr: 2,
    gravity: 22,
    jumpSpeed: 8.2,
  };

  private readonly debugTools: DebugTools;
  private pauseShare: PauseShare | null = null;
  private session: Session = defaultSession();
  private chapter: Chapter = this.session.chapters[0];
  private floor: THREE.Mesh | null = null;
  private sun: THREE.DirectionalLight | null = null;
  private world: WorldKit | null = null;
  private hazard: Hazard | null = null;
  private worldRoot: THREE.Group | null = null;
  private frame = 0;
  private score = 0;
  private elapsed = 0;
  private complete = false;
  private failed = false;
  private cameraPunch = 0;
  private rng = createSeededRandom(1);
  private pausedForScreenshot = false;
  private reducedMotion = false;

  constructor(private readonly canvas: HTMLCanvasElement) {
    this.renderer = createRenderer(canvas);
    this.renderer.toneMappingExposure = this.tuning.exposure;

    const stick = this.getElement('#touch-stick');
    const knob = this.getElement('#touch-knob');
    const dashButton = this.getElement('#dash-button');
    this.input = new InputController(stick, knob, dashButton);

    this.debugTools = new DebugTools(this.tuning, () => {
      this.renderer.toneMappingExposure = this.tuning.exposure;
      resizeRenderer(this.renderer, this.camera, this.tuning.maxDpr);
    });

    this.createScene();
    this.rebuildEntities();
    this.hud.setTarget(this.pickups.length);
    this.cameraRig.snapTo(this.player.group.position);
    resizeRenderer(this.renderer, this.camera, this.tuning.maxDpr);
    this.pauseShare = new PauseShare(
      {
        onResume: () => undefined,
        onReplay: () => this.resetRun(),
      },
      this.session.title,
    );
    this.installTestHooks();
    this.publishDiagnostics();
    void this.bootLook();
  }

  start(): void {
    this.loop.start();
  }

  dispose(): void {
    this.loop.stop();
    this.input.dispose();
    this.audio.dispose();
    this.debugTools.dispose();
    this.pauseShare?.dispose();
    this.clearEntities();
    this.player.dispose();
    this.renderer.dispose();
    window.__THREE_GAME_DIAGNOSTICS__ = undefined;
    window.__THREE_GAME_TEST_HOOKS__ = undefined;
  }

  private async bootLook(): Promise<void> {
    this.session = await this.look.load('/look/look.json');
    this.rebuildEntities();
    if (this.floor) await this.look.apply(this.scene, this.floor, this.session);
    await this.player.applyCast(this.session.models?.player);
    await Promise.all(this.pickups.map((pickup) => pickup.applyCast(this.session.models?.pickup)));
    if (this.hazard) await this.hazard.applyCast(this.session.models?.enemy);
    this.applyChapter(chapterFromScore(this.session, this.score, this.complete), true);
    const title = document.querySelector('title');
    if (title) title.textContent = this.session.title;
  }

  private update(delta: number, elapsed: number): void {
    this.frame += 1;
    const paused = this.pauseShare?.paused || this.pausedForScreenshot;
    if (paused) {
      this.publishDiagnostics();
      return;
    }
    if (!this.complete && !this.failed) this.elapsed += delta;

    resizeRenderer(this.renderer, this.camera, this.tuning.maxDpr);
    const animDelta = this.reducedMotion ? 0 : delta;
    const animElapsed = this.reducedMotion ? 0 : elapsed;
    const air = this.session.cartridge === 'jump';
    this.player.update(delta, animElapsed, this.input, { ...this.tuning, air }, ARENA);
    if (air) this.resolveJumpSupport();

    for (const pickup of this.pickups) {
      pickup.update(animDelta, animElapsed);
    }
    this.hazard?.update(animDelta, animElapsed);

    if (!this.complete && !this.failed) {
      const collected = this.collision.collectPickups(this.player.group.position, this.pickups, 0.55);
      for (const pickup of collected) {
        this.score += 1;
        this.audio.pickup(pickup.index);
        this.hud.flashPickup();
        this.cameraPunch = 0.28;
        this.renderer.toneMappingExposure = this.tuning.exposure + 0.1;
      }

      if (this.hazard?.hits(this.player.group.position)) {
        this.failRun();
      }
      if (air && this.player.group.position.y < -4) {
        this.failRun();
      }

      if (this.score >= this.pickups.length && this.pickups.length > 0) {
        this.complete = true;
        this.pauseShare?.showSettle(true);
      }
    }

    this.applyChapter(chapterFromScore(this.session, this.score, this.complete));
    this.cameraRig.update(delta, this.player.group.position, this.tuning.cameraLag);
    if (this.cameraPunch > 0.002) {
      this.camera.position.y += this.cameraPunch;
      this.cameraPunch *= 0.72;
    }
    this.renderer.toneMappingExposure += (this.tuning.exposure - this.renderer.toneMappingExposure) * 0.12;
    this.hud.update(this.score, this.pickups.length, this.elapsed, this.complete, this.chapter.status);
    this.publishDiagnostics();
  }

  private resolveJumpSupport(): void {
    if (!this.world) return;
    const pos = this.player.group.position;
    const top = supportY(this.world.platforms, pos.x, pos.z, pos.y);
    if (top !== null && this.player.velocity.y <= 0 && pos.y <= top + 0.12) {
      this.player.setGrounded(true, top);
    } else {
      this.player.setGrounded(false);
    }
  }

  private failRun(): void {
    if (this.failed || this.complete) return;
    this.failed = true;
    this.audio.fail();
    this.pauseShare?.showSettle(true);
  }

  private applyChapter(next: Chapter, force = false): void {
    if (!force && this.chapter.id === next.id) return;
    this.chapter = next;
    if (this.floor && this.sun) this.look.applyChapter(this.scene, this.floor, this.sun, next);
    this.hud.setChapter(next);
  }

  private render(): void {
    this.renderer.render(this.scene, this.camera);
  }

  private createScene(): void {
    this.scene.background = new THREE.Color('#151713');
    this.scene.fog = new THREE.Fog('#151713', 20, 44);

    const hemisphere = new THREE.HemisphereLight('#f6f1df', '#2b322d', 1.7);
    this.scene.add(hemisphere);

    const sun = new THREE.DirectionalLight('#fff1bf', 2.6);
    sun.position.set(-5, 9, 6);
    sun.castShadow = true;
    sun.shadow.mapSize.set(2048, 2048);
    sun.shadow.camera.near = 0.5;
    sun.shadow.camera.far = 30;
    sun.shadow.camera.left = -14;
    sun.shadow.camera.right = 14;
    sun.shadow.camera.top = 12;
    sun.shadow.camera.bottom = -12;
    this.scene.add(sun);
    this.sun = sun;
    this.scene.add(this.player.group);
  }

  private rebuildEntities(): void {
    this.clearEntities();
    this.world = createWorldKit(this.session.cartridge, this.session);
    this.worldRoot = this.world.group;
    this.floor = this.world.floor;
    this.scene.add(this.world.group);

    const spots = pickupLayout(this.session);
    spots.forEach((spot, index) => {
      const position = this.pickupPosition(spot, index);
      const pickup = new Pickup(index, position);
      this.pickups.push(pickup);
      this.scene.add(pickup.group);
    });

    if (this.session.threat) {
      this.hazard = new Hazard();
      this.scene.add(this.hazard.group);
    }

    this.hud.setTarget(this.pickups.length);
    if (this.session.cartridge === 'jump' && this.world?.platforms[0]) {
      this.player.setGrounded(true, this.world.platforms[0].top);
    }
  }

  private pickupPosition(spot: { x: number; z: number }, index: number): THREE.Vector3 {
    if (this.session.cartridge !== 'jump' || !this.world?.platforms.length) {
      return new THREE.Vector3(spot.x, 0.8, spot.z);
    }
    const platform = this.world.platforms[index % this.world.platforms.length];
    const midX = (platform.minX + platform.maxX) / 2;
    const midZ = (platform.minZ + platform.maxZ) / 2;
    const offset = index % 2 === 0 ? 0.55 : -0.55;
    return new THREE.Vector3(midX + offset, platform.top + 0.55, midZ);
  }

  private clearEntities(): void {
    for (const pickup of this.pickups) {
      this.scene.remove(pickup.group);
      pickup.dispose();
    }
    this.pickups.length = 0;
    if (this.hazard) {
      this.scene.remove(this.hazard.group);
      this.hazard.dispose();
      this.hazard = null;
    }
    if (this.worldRoot) {
      this.scene.remove(this.worldRoot);
      this.worldRoot = null;
    }
    this.world = null;
    this.floor = null;
  }

  private installTestHooks(): void {
    window.__THREE_GAME_TEST_HOOKS__ = {
      seed: (value: number) => {
        this.rng = createSeededRandom(value);
      },
      setState: (name: string) => {
        if (name === 'active-play') this.resetRun();
        else if (name === 'complete') this.completeRun();
        else console.warn(`Unknown test state: ${name}`);
      },
      setPausedForScreenshot: (paused: boolean) => {
        this.pausedForScreenshot = paused;
      },
      setReducedMotion: (enabled: boolean) => {
        this.reducedMotion = enabled;
      },
      hideDebugUi: (hidden: boolean) => {
        this.debugTools.setHidden(hidden);
      },
    };
  }

  private resetRun(): void {
    this.score = 0;
    this.elapsed = 0;
    this.complete = false;
    this.failed = false;
    this.cameraPunch = 0;
    this.player.group.position.set(0, this.session.cartridge === 'jump' ? 0.6 : 0, 0);
    this.player.velocity.set(0, 0, 0);
    this.player.setGrounded(true, this.session.cartridge === 'jump' ? 0.55 : 0);
    for (const pickup of this.pickups) {
      pickup.reset();
      pickup.group.rotation.y = this.rng() * Math.PI * 2;
    }
    this.cameraRig.snapTo(this.player.group.position);
    this.hud.setTarget(this.pickups.length);
    this.applyChapter(chapterFromScore(this.session, this.score, this.complete), true);
    this.hud.update(this.score, this.pickups.length, this.elapsed, this.complete, this.chapter.status);
    this.pauseShare?.showSettle(false);
    this.pauseShare?.setPaused(false);
  }

  private completeRun(): void {
    for (const pickup of this.pickups) {
      if (pickup.active) pickup.collect();
    }
    this.score = this.pickups.length;
    this.complete = true;
    this.failed = false;
    this.applyChapter(chapterFromScore(this.session, this.score, this.complete), true);
    this.hud.update(this.score, this.pickups.length, this.elapsed, this.complete, this.chapter.status);
    this.pauseShare?.showSettle(true);
  }

  private publishDiagnostics(): void {
    const info = this.renderer.info;
    window.__THREE_GAME_DIAGNOSTICS__ = {
      frame: this.frame,
      elapsed: this.elapsed,
      score: this.score,
      targetScore: this.pickups.length,
      complete: this.complete,
      failed: this.failed,
      chapter: this.chapter.id,
      paused: Boolean(this.pauseShare?.paused),
      player: {
        position: {
          x: this.player.group.position.x,
          y: this.player.group.position.y,
          z: this.player.group.position.z,
        },
        speed: this.player.velocity.length(),
      },
      renderer: {
        calls: info.render.calls,
        triangles: info.render.triangles,
        geometries: info.memory.geometries,
        textures: info.memory.textures,
      },
      canvas: {
        clientWidth: this.canvas.clientWidth,
        clientHeight: this.canvas.clientHeight,
        width: this.canvas.width,
        height: this.canvas.height,
        dpr: Math.min(window.devicePixelRatio || 1, this.tuning.maxDpr),
      },
    };
  }

  private getElement(selector: string): HTMLElement {
    const element = document.querySelector<HTMLElement>(selector);
    if (!element) throw new Error(`Missing element: ${selector}`);
    return element;
  }
}
