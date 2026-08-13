import * as THREE from 'three';
import { ARENA, platformLayout, type Cartridge, type Session } from '../studio/session';

export type WorldKit = {
  group: THREE.Group;
  floor: THREE.Mesh;
  platforms: Array<{ top: number; minX: number; maxX: number; minZ: number; maxZ: number }>;
};

export function createWorldKit(cartridge: Cartridge, session: Session): WorldKit {
  return cartridge === 'jump' ? createJumpWorld(session) : createCollectWorld();
}

function wood(): THREE.MeshStandardMaterial {
  return new THREE.MeshStandardMaterial({ color: '#8a5a32', roughness: 0.62, metalness: 0.04 });
}

function stone(): THREE.MeshStandardMaterial {
  return new THREE.MeshStandardMaterial({ color: '#5c5850', roughness: 0.84, metalness: 0.08 });
}

function moss(): THREE.MeshStandardMaterial {
  return new THREE.MeshStandardMaterial({ color: '#3d4a32', roughness: 0.9, metalness: 0.02 });
}

function addMesh(
  group: THREE.Group,
  geometry: THREE.BufferGeometry,
  material: THREE.Material,
  x: number,
  y: number,
  z: number,
  rotY = 0,
): THREE.Mesh {
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.set(x, y, z);
  mesh.rotation.y = rotY;
  mesh.castShadow = true;
  mesh.receiveShadow = true;
  group.add(mesh);
  return mesh;
}

function createFloor(width: number, depth: number): THREE.Mesh {
  const floor = new THREE.Mesh(
    new THREE.PlaneGeometry(width, depth, 1, 1),
    new THREE.MeshStandardMaterial({
      color: '#2a2c25',
      roughness: 0.72,
      metalness: 0.02,
    }),
  );
  floor.rotation.x = -Math.PI / 2;
  floor.receiveShadow = true;
  return floor;
}

function createCollectWorld(): WorldKit {
  const group = new THREE.Group();
  const floor = createFloor(ARENA.halfWidth * 2, ARENA.halfDepth * 2);
  group.add(floor);

  const rail = wood();
  const longRail = new THREE.BoxGeometry(ARENA.halfWidth * 2 + 1, 0.55, 0.42);
  const shortRail = new THREE.BoxGeometry(0.42, 0.55, ARENA.halfDepth * 2 + 1);
  addMesh(group, longRail, rail, 0, 0.28, -ARENA.halfDepth - 0.24);
  addMesh(group, longRail, rail, 0, 0.28, ARENA.halfDepth + 0.24);
  addMesh(group, shortRail, rail, -ARENA.halfWidth - 0.24, 0.28, 0);
  addMesh(group, shortRail, rail, ARENA.halfWidth + 0.24, 0.28, 0);

  const crate = new THREE.BoxGeometry(0.9, 0.7, 0.9);
  const post = new THREE.CylinderGeometry(0.12, 0.16, 1.6, 8);
  const boulder = new THREE.SphereGeometry(0.48, 8, 6);
  const lantern = new THREE.BoxGeometry(0.22, 0.38, 0.22);
  const dress = moss();
  const rock = stone();
  addMesh(group, crate, dress, -9.2, 0.35, -5.1, 0.2);
  addMesh(group, crate, dress, 9.4, 0.35, 4.8, -0.3);
  addMesh(group, crate, dress, -8.6, 0.35, 5.2, 0.4);
  addMesh(group, post, rail, -10.2, 0.8, -2.2);
  addMesh(group, post, rail, 10.2, 0.8, 1.6);
  addMesh(group, post, rail, 4.2, 0.8, -6.4);
  addMesh(group, boulder, rock, -5.4, 0.28, 5.8);
  addMesh(group, boulder, rock, 6.8, 0.28, -5.6, 0.6);
  addMesh(group, boulder, rock, 1.2, 0.22, 6.1);
  addMesh(group, lantern, new THREE.MeshStandardMaterial({ color: '#d9b36a', emissive: '#7a4a12', emissiveIntensity: 0.55, roughness: 0.4 }), -9.2, 1.05, -5.1);
  addMesh(group, lantern, new THREE.MeshStandardMaterial({ color: '#d9b36a', emissive: '#7a4a12', emissiveIntensity: 0.55, roughness: 0.4 }), 9.4, 1.05, 4.8);

  return {
    group,
    floor,
    platforms: [
      {
        top: 0,
        minX: -ARENA.halfWidth,
        maxX: ARENA.halfWidth,
        minZ: -ARENA.halfDepth,
        maxZ: ARENA.halfDepth,
      },
    ],
  };
}

function createJumpWorld(session: Session): WorldKit {
  const group = new THREE.Group();
  const floor = createFloor(48, 48);
  floor.position.y = -8;
  const floorMat = floor.material as THREE.MeshStandardMaterial;
  floorMat.color.set('#12110f');
  group.add(floor);

  const platforms: WorldKit['platforms'] = [];
  const specs = platformLayout(session);
  const rock = stone();
  for (const spec of specs) {
    const mesh = addMesh(
      group,
      new THREE.BoxGeometry(spec.w, spec.h, spec.d),
      rock,
      spec.x,
      spec.y + spec.h / 2,
      spec.z,
    );
    if (spec.x === 0 && spec.z === 0) {
      mesh.material = new THREE.MeshStandardMaterial({ color: '#2a2c25', roughness: 0.72, metalness: 0.02 });
    }
    platforms.push({
      top: spec.y + spec.h,
      minX: spec.x - spec.w / 2,
      maxX: spec.x + spec.w / 2,
      minZ: spec.z - spec.d / 2,
      maxZ: spec.z + spec.d / 2,
    });
  }

  return { group, floor, platforms };
}

export function supportY(
  platforms: WorldKit['platforms'],
  x: number,
  z: number,
  y: number,
): number | null {
  let best: number | null = null;
  for (const platform of platforms) {
    if (x < platform.minX || x > platform.maxX || z < platform.minZ || z > platform.maxZ) continue;
    if (y + 0.35 < platform.top) continue;
    if (best === null || platform.top > best) best = platform.top;
  }
  return best;
}
