import type { AudioPosition } from "../types/models.ts";

export function startingBlockSeconds(
  position: AudioPosition | null | undefined,
  chapter: number,
  blockId: string,
  profileId: string,
): number | undefined {
  if (
    position?.chapter !== chapter ||
    position.blockId !== blockId ||
    position.profileId !== profileId ||
    !Number.isFinite(position.seconds) ||
    position.seconds < 0
  )
    return undefined;
  return position.seconds;
}
