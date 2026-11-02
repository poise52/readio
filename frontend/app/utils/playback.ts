export function playbackRate(
  baseRate: number,
  synthesizedSpeed: number,
  desiredSpeed: number,
): number {
  return Math.min(
    16,
    Math.max(0.0625, (baseRate * desiredSpeed) / synthesizedSpeed),
  );
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

export function configurePlaybackRate(
  audio: { playbackRate: number; preservesPitch: boolean },
  requestedRate?: number,
): void {
  audio.playbackRate = requestedRate ?? 1;
  audio.preservesPitch = true;
}
