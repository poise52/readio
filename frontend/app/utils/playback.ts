export interface PlaybackHooks<T> {
  current(): T | null;
  text(item: T): string;
  before(item: T): void;
  speak(text: string): Promise<void>;
  advance(item: T): Promise<boolean>;
  isCurrent(): boolean;
}

export async function runPlayback<T>(hooks: PlaybackHooks<T>): Promise<void> {
  while (hooks.isCurrent()) {
    const item = hooks.current();
    if (!item) return;
    hooks.before(item);
    const text = hooks.text(item);
    if (/[\p{L}\p{N}]/u.test(text)) await hooks.speak(text);
    if (!hooks.isCurrent() || !(await hooks.advance(item))) return;
  }
}

export function replayIndex(completed: boolean, active: number): number {
  return completed ? 0 : active;
}

export async function synthesizeThenPlay<T>(
  synthesize: () => Promise<T>,
  play: (audio: T) => Promise<void>,
  isCurrent: () => boolean,
): Promise<void> {
  if (!isCurrent()) return;
  const audio = await synthesize();
  if (isCurrent()) await play(audio);
}
