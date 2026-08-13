import * as THREE from 'three';
import type { InputController } from '../core/InputController';

export type PlayerTuning = {
  speed: number;
  dashMultiplier: number;
  acceleration: number;
};

export type ArenaBounds = {
  halfWidth: number;
  halfDepth: number;
};

export class Player {
  readonly group = new THREE.Group();
  readonly velocity = new THREE.Vector3();

  private readonly move = new THREE.Vector2();
  private readonly targetVelocity = new THREE.Vector3();
  private readonly cloth = new THREE.MeshStandardMaterial({
    color: '#c45c2a',
    roughness: 0.62,
    metalness: 0.04,
  });
  private readonly skin = new THREE.MeshStandardMaterial({
    color: '#e6c29a',
    roughness: 0.55,
    metalness: 0.02,
  });
  private readonly accent = new THREE.MeshStandardMaterial({
    color: '#f2d36b',
    emissive: '#8a5a12',
    emissiveIntensity: 0.7,
    roughness: 0.28,
    metalness: 0.12,
  });
  private readonly torsoGeometry = new THREE.BoxGeometry(0.62, 0.72, 0.36);
  private readonly cloakGeometry = new THREE.BoxGeometry(0.78, 0.86, 0.16);
  private readonly headGeometry = new THREE.SphereGeometry(0.22, 12, 10);
  private readonly emblemGeometry = new THREE.SphereGeometry(0.12, 10, 8);
  private readonly legGeometry = new THREE.BoxGeometry(0.16, 0.42, 0.18);

  constructor() {
    const torso = new THREE.Mesh(this.torsoGeometry, this.cloth);
    torso.castShadow = true;
    torso.receiveShadow = true;
    torso.position.y = 0.86;
    this.group.add(torso);

    const cloak = new THREE.Mesh(this.cloakGeometry, this.cloth);
    cloak.castShadow = true;
    cloak.position.set(0, 0.78, 0.22);
    cloak.rotation.x = 0.18;
    this.group.add(cloak);

    const head = new THREE.Mesh(this.headGeometry, this.skin);
    head.castShadow = true;
    head.position.y = 1.36;
    this.group.add(head);

    const emblem = new THREE.Mesh(this.emblemGeometry, this.accent);
    emblem.castShadow = true;
    emblem.position.set(0.38, 0.92, -0.18);
    this.group.add(emblem);

    const leftLeg = new THREE.Mesh(this.legGeometry, this.cloth);
    const rightLeg = new THREE.Mesh(this.legGeometry, this.cloth);
    leftLeg.castShadow = true;
    rightLeg.castShadow = true;
    leftLeg.position.set(-0.16, 0.28, 0);
    rightLeg.position.set(0.16, 0.28, 0);
    this.group.add(leftLeg, rightLeg);
  }

  update(delta: number, elapsed: number, input: InputController, tuning: PlayerTuning, bounds: ArenaBounds): void {
    input.readMovement(this.move);
    const dash = input.isDashHeld() ? tuning.dashMultiplier : 1;
    this.targetVelocity.set(this.move.x, 0, this.move.y).multiplyScalar(tuning.speed * dash);

    const smoothing = 1 - Math.exp(-tuning.acceleration * delta);
    this.velocity.lerp(this.targetVelocity, smoothing);
    this.group.position.addScaledVector(this.velocity, delta);

    this.group.position.x = THREE.MathUtils.clamp(this.group.position.x, -bounds.halfWidth + 0.8, bounds.halfWidth - 0.8);
    this.group.position.z = THREE.MathUtils.clamp(this.group.position.z, -bounds.halfDepth + 0.8, bounds.halfDepth - 0.8);

    if (this.velocity.lengthSq() > 0.001) {
      this.group.rotation.y = Math.atan2(this.velocity.x, -this.velocity.z);
    }

    this.group.position.y = 0.06 + Math.sin(elapsed * 9) * Math.min(this.velocity.length() / 40, 0.08);
  }

  dispose(): void {
    this.torsoGeometry.dispose();
    this.cloakGeometry.dispose();
    this.headGeometry.dispose();
    this.emblemGeometry.dispose();
    this.legGeometry.dispose();
    this.cloth.dispose();
    this.skin.dispose();
    this.accent.dispose();
  }
}
