import * as THREE from 'three';
import { FBXLoader } from 'three/addons/loaders/FBXLoader.js';
import { GLTFLoader } from 'three/addons/loaders/GLTFLoader.js';

export type CastSlot = {
  file: string;
  walk?: string;
  run?: string;
  height?: number;
};

export type CastActions = {
  idle?: THREE.AnimationAction;
  walk?: THREE.AnimationAction;
  run?: THREE.AnimationAction;
};

export type CastVisual = {
  group: THREE.Group;
  mixer: THREE.AnimationMixer;
  actions: CastActions;
};

export function stripRootXz(clips: THREE.AnimationClip[]): void {
  for (const clip of clips) {
    for (const track of clip.tracks) {
      if (track.name !== 'Root.position') continue;
      const values = track.values;
      const x0 = values[0];
      const z0 = values[2];
      for (let i = 0; i < values.length; i += 3) {
        values[i] = x0;
        values[i + 2] = z0;
      }
    }
  }
}

function publicUrl(file: string): string {
  return file.startsWith('/') ? file : `/${file}`;
}

function pickShallowClip(clips: THREE.AnimationClip[]): THREE.AnimationClip | undefined {
  if (!clips.length) return undefined;
  return [...clips].sort((a, b) => {
    const depthA = a.tracks[0]?.name.split('|').length ?? 0;
    const depthB = b.tracks[0]?.name.split('|').length ?? 0;
    return depthA - depthB;
  })[0];
}

async function loadObject(url: string): Promise<{ root: THREE.Object3D; clips: THREE.AnimationClip[] }> {
  if (/\.fbx$/i.test(url)) {
    const fbx = await new FBXLoader().loadAsync(url);
    return { root: fbx, clips: fbx.animations ?? [] };
  }
  const gltf = await new GLTFLoader().loadAsync(url);
  return { root: gltf.scene, clips: gltf.animations ?? [] };
}

function fitToHeight(root: THREE.Object3D, height: number): void {
  const box = new THREE.Box3().setFromObject(root);
  const size = new THREE.Vector3();
  box.getSize(size);
  if (size.y <= 0.001) return;
  root.scale.multiplyScalar(height / size.y);
  const fitted = new THREE.Box3().setFromObject(root);
  root.position.y -= fitted.min.y;
}

export async function loadCastVisual(slot: CastSlot): Promise<CastVisual | null> {
  try {
    const { root, clips } = await loadObject(publicUrl(slot.file));
    stripRootXz(clips);
    fitToHeight(root, slot.height ?? 1.7);
    root.traverse((obj) => {
      if ((obj as THREE.Mesh).isMesh) {
        obj.castShadow = true;
        obj.receiveShadow = true;
      }
    });
    const group = new THREE.Group();
    group.add(root);
    const mixer = new THREE.AnimationMixer(root);
    const actions: CastActions = {};
    const idle = pickShallowClip(clips);
    if (idle) {
      actions.idle = mixer.clipAction(idle);
      actions.idle.play();
    }
    if (slot.walk) {
      const extra = await loadObject(publicUrl(slot.walk));
      stripRootXz(extra.clips);
      const clip = pickShallowClip(extra.clips);
      if (clip) actions.walk = mixer.clipAction(clip);
    }
    if (slot.run) {
      const extra = await loadObject(publicUrl(slot.run));
      stripRootXz(extra.clips);
      const clip = pickShallowClip(extra.clips);
      if (clip) actions.run = mixer.clipAction(clip);
    }
    return { group, mixer, actions };
  } catch {
    return null;
  }
}
