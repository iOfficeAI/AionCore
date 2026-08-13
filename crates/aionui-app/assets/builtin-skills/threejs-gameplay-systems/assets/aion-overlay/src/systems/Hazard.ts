import * as THREE from 'three';
import { loadCastVisual, type CastSlot, type CastVisual } from '../studio/cast';

export class Hazard {
  readonly group = new THREE.Group();
  readonly radius = 0.7;
  private readonly bodyGeometry = new THREE.BoxGeometry(0.7, 1.1, 0.46);
  private readonly bodyMaterial = new THREE.MeshStandardMaterial({
    color: '#6b2c28',
    roughness: 0.55,
    metalness: 0.08,
  });
  private cast: CastVisual | null = null;
  private readonly origin = new THREE.Vector3();
  private readonly span = 5.5;

  constructor() {
    const body = new THREE.Mesh(this.bodyGeometry, this.bodyMaterial);
    body.castShadow = true;
    body.position.y = 0.55;
    this.group.add(body);
    this.origin.set(-5.4, 0, 0);
    this.group.position.copy(this.origin);
  }

  async applyCast(slot?: CastSlot): Promise<void> {
    if (!slot?.file) return;
    const visual = await loadCastVisual({ ...slot, height: slot.height ?? 1.5 });
    if (!visual) return;
    for (const child of this.group.children) child.visible = false;
    this.group.add(visual.group);
    this.cast = visual;
  }

  update(delta: number, elapsed: number): void {
    this.group.position.x = this.origin.x + Math.sin(elapsed * 0.55) * this.span;
    this.group.rotation.y = this.group.position.x >= this.origin.x ? Math.PI / 2 : -Math.PI / 2;
    this.cast?.mixer.update(delta);
  }

  hits(position: THREE.Vector3): boolean {
    const dx = position.x - this.group.position.x;
    const dz = position.z - this.group.position.z;
    return dx * dx + dz * dz <= this.radius * this.radius;
  }

  dispose(): void {
    this.bodyGeometry.dispose();
    this.bodyMaterial.dispose();
  }
}
