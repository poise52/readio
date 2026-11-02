export class PreviewOwner {
  private token: number | null = null;
  private pending: Promise<void> = Promise.resolve();
  private stopNative: () => Promise<void>;
  private onError: (error: unknown) => void;
  constructor(
    stopNative: () => Promise<void>,
    onError: (error: unknown) => void,
  ) {
    this.stopNative = stopNative;
    this.onError = onError;
  }
  get active(): boolean {
    return this.token !== null;
  }
  start(token: number): void {
    this.token = token;
  }
  finish(token: number): void {
    if (this.token === token) this.token = null;
  }
  cancel(): void {
    if (this.token === null) return;
    this.token = null;
    this.pending = this.pending.then(this.stopNative).catch(this.onError);
  }
  wait(): Promise<void> {
    return this.pending;
  }
}
