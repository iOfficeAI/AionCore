import { sharePayload } from '../studio/session';

type PauseShareHandlers = {
  onResume: () => void;
  onReplay: () => void;
};

export class PauseShare {
  private readonly pauseOverlay = this.getElement('#pause-overlay');
  private readonly settleOverlay = this.getElement('#settle-overlay');
  private readonly shareStatus = document.querySelector<HTMLElement>('#share-status');
  private readonly onKey = (event: KeyboardEvent) => {
    if (event.code !== 'Escape' || event.repeat) return;
    if (this.settleOverlay.hidden) this.setPaused(this.pauseOverlay.hidden);
  };

  constructor(
    private readonly handlers: PauseShareHandlers,
    private readonly title: string,
  ) {
    this.getElement('#resume-button').addEventListener('click', () => this.setPaused(false));
    this.getElement('#replay-button').addEventListener('click', () => {
      this.showSettle(false);
      this.handlers.onReplay();
    });
    this.getElement('#share-button').addEventListener('click', () => void this.share());
    this.getElement('#settle-share-button').addEventListener('click', () => void this.share());
    window.addEventListener('keydown', this.onKey);
  }

  get paused(): boolean {
    return !this.pauseOverlay.hidden;
  }

  setPaused(paused: boolean): void {
    this.pauseOverlay.hidden = !paused;
    if (!paused) this.handlers.onResume();
  }

  showSettle(show: boolean): void {
    this.settleOverlay.hidden = !show;
    if (show) this.pauseOverlay.hidden = true;
  }

  async share(): Promise<void> {
    const payload = sharePayload(window.location.href, this.title);
    try {
      if (navigator.share && !payload.local) {
        await navigator.share({ title: this.title, url: payload.url, text: payload.text });
      } else if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(payload.url);
      }
      this.setStatus(payload.local ? payload.text : 'Link copied');
    } catch {
      this.setStatus(payload.local ? payload.text : 'Share failed');
    }
  }

  dispose(): void {
    window.removeEventListener('keydown', this.onKey);
  }

  private setStatus(text: string): void {
    if (this.shareStatus) this.shareStatus.textContent = text;
    const settleStatus = document.querySelector<HTMLElement>('#settle-share-status');
    if (settleStatus) settleStatus.textContent = text;
  }

  private getElement(selector: string): HTMLElement {
    const element = document.querySelector<HTMLElement>(selector);
    if (!element) throw new Error(`Missing overlay element: ${selector}`);
    return element;
  }
}
