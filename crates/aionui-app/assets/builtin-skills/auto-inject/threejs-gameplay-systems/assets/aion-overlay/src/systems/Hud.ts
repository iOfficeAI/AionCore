import type { Chapter } from '../studio/session';

export class Hud {
  private readonly scoreValue = this.getElement('#score-value');
  private readonly targetValue = this.getElement('#target-value');
  private readonly timerValue = this.getElement('#timer-value');
  private readonly statusLine = this.getElement('#status-line');
  private readonly chapterLine = document.querySelector<HTMLElement>('#chapter-line');

  setTarget(target: number): void {
    this.targetValue.textContent = String(target);
  }

  setChapter(chapter: Chapter): void {
    if (this.chapterLine) this.chapterLine.textContent = chapter.name;
    if (!this.statusLine.dataset.complete) {
      this.statusLine.textContent = chapter.status;
    }
  }

  update(score: number, target: number, elapsed: number, complete: boolean, status?: string): void {
    this.scoreValue.textContent = String(score);
    this.targetValue.textContent = String(target);
    const minutes = Math.floor(elapsed / 60).toString().padStart(2, '0');
    const seconds = Math.floor(elapsed % 60).toString().padStart(2, '0');
    this.timerValue.textContent = `${minutes}:${seconds}`;
    this.statusLine.dataset.complete = complete ? '1' : '';
    this.statusLine.textContent = complete ? 'Brought home' : status || this.statusLine.textContent;
  }

  flashPickup(): void {
    this.statusLine.animate(
      [
        { transform: 'translateY(0)', borderLeftColor: '#f5ba49' },
        { transform: 'translateY(-3px)', borderLeftColor: '#48baa7' },
        { transform: 'translateY(0)', borderLeftColor: '#f5ba49' },
      ],
      { duration: 220, easing: 'ease-out' },
    );
  }

  private getElement(selector: string): HTMLElement {
    const element = document.querySelector<HTMLElement>(selector);
    if (!element) throw new Error(`Missing HUD element: ${selector}`);
    return element;
  }
}
