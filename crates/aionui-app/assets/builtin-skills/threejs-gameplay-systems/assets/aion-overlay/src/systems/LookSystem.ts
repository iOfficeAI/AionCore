import * as THREE from 'three';
import { defaultSession, type Chapter, type Session } from '../studio/session';

export class LookSystem {
  private skyLoaded = false;

  async load(url = '/look/look.json'): Promise<Session> {
    try {
      const response = await fetch(url);
      if (!response.ok) return defaultSession();
      const data = (await response.json()) as Session;
      return {
        ...defaultSession(),
        ...data,
        chapters: data.chapters?.length ? data.chapters : defaultSession().chapters,
        look: { ...defaultSession().look, ...data.look },
        models: { ...defaultSession().models, ...data.models },
      };
    } catch {
      return defaultSession();
    }
  }

  async apply(
    scene: THREE.Scene,
    floor: THREE.Mesh,
    session: Session,
  ): Promise<void> {
    const icon = document.querySelector<HTMLImageElement>('#hud-icon');
    if (icon && session.look.icon) {
      icon.src = this.publicUrl(session.look.icon);
      icon.hidden = false;
    }
    await Promise.all([
      this.tryTexture(session.look.sky, (texture) => {
        texture.mapping = THREE.EquirectangularReflectionMapping;
        scene.background = texture;
        this.skyLoaded = true;
      }),
      this.tryTexture(session.look.ground, (texture) => {
        texture.wrapS = THREE.RepeatWrapping;
        texture.wrapT = THREE.RepeatWrapping;
        texture.repeat.set(6, 4);
        const material = floor.material as THREE.MeshStandardMaterial;
        material.map = texture;
        material.needsUpdate = true;
      }),
    ]);
  }

  applyChapter(scene: THREE.Scene, floor: THREE.Mesh, sun: THREE.DirectionalLight, chapter: Chapter): void {
    if (chapter.fog) {
      const fogColor = new THREE.Color(chapter.fog);
      if (scene.fog instanceof THREE.Fog) scene.fog.color.copy(fogColor);
      if (!this.skyLoaded) scene.background = fogColor;
    }
    if (chapter.sun) sun.color.set(chapter.sun);
    if (chapter.ground) {
      const material = floor.material as THREE.MeshStandardMaterial;
      material.color.set(chapter.ground);
    }
  }

  private publicUrl(file: string): string {
    return file.startsWith('/') ? file : `/${file}`;
  }

  private tryTexture(file: string | undefined, apply: (texture: THREE.Texture) => void): Promise<void> {
    if (!file) return Promise.resolve();
    const loader = new THREE.TextureLoader();
    return new Promise((resolve) => {
      loader.load(
        this.publicUrl(file),
        (texture) => {
          texture.colorSpace = THREE.SRGBColorSpace;
          apply(texture);
          resolve();
        },
        undefined,
        () => resolve(),
      );
    });
  }
}
