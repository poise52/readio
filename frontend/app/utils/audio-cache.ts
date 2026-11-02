import type { AudioBlock, SpeechAudio } from "../types/models";
import type { PlaybackRequest } from "./playback-interest";

export function audioErrorCode(error: unknown): string | undefined {
  return typeof error === "object" &&
    error !== null &&
    "code" in error &&
    typeof error.code === "string"
    ? error.code
    : undefined;
}

export function audioErrorMessage(error: unknown): string {
  return typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
    ? error.message
    : String(error);
}

export class AudioBlockLoader {
  private pending = new Map<string, Promise<SpeechAudio>>();
  private ready = new Map<string, SpeechAudio>();
  private readyBytes = 0;
  private readonly maxReadyBytes = 16 * 1024 * 1024;
  private readonly maxFailures = 1024;
  private invalidated = new Set<string>();
  private prefetchPending = false;
  private failed = new Map<string, unknown>();
  private fetchBlock: (
    key: string,
    retry: boolean,
    context?: PlaybackRequest,
  ) => Promise<SpeechAudio>;
  private capacity: number;
  constructor(
    fetchBlock: (
      key: string,
      retry: boolean,
      context?: PlaybackRequest,
    ) => Promise<SpeechAudio>,
    capacity = 3,
  ) {
    this.fetchBlock = fetchBlock;
    this.capacity = capacity;
  }
  get(
    key: string,
    retry = false,
    context?: PlaybackRequest,
  ): Promise<SpeechAudio> {
    const cached = this.ready.get(key);
    if (cached) {
      this.ready.delete(key);
      this.ready.set(key, cached);
      return Promise.resolve(cached);
    }
    const inFlight = this.pending.get(key);
    if (inFlight) return inFlight;
    if (this.failed.has(key)) {
      const failure = this.failed.get(key);
      if (
        !retry ||
        (audioErrorCode(failure) === "retry_required" &&
          !context?.confirmedAttempt)
      )
        return Promise.reject(failure);
    }
    if (retry) this.failed.delete(key);
    this.invalidated.delete(key);
    const request = this.fetchBlock(key, retry, context)
      .then((audio) => {
        if (this.invalidated.delete(key)) return audio;
        const size = audio.data.length * 2;
        if (size <= this.maxReadyBytes && this.capacity > 0) {
          this.ready.set(key, audio);
          this.readyBytes += size;
          while (
            this.ready.size > this.capacity ||
            this.readyBytes > this.maxReadyBytes
          ) {
            const oldest = this.ready.keys().next().value!;
            this.readyBytes -= this.ready.get(oldest)!.data.length * 2;
            this.ready.delete(oldest);
          }
        }
        return audio;
      })
      .catch((error: unknown) => {
        if (
          !this.invalidated.has(key) &&
          audioErrorCode(error) !== "cancelled"
        ) {
          this.failed.set(key, error);
          while (this.failed.size > this.maxFailures)
            this.failed.delete(this.failed.keys().next().value!);
        }
        throw error;
      })
      .finally(() => {
        this.pending.delete(key);
        this.invalidated.delete(key);
      });
    this.pending.set(key, request);
    return request;
  }
  clearBook(bookId: string): void {
    const belongs = (key: string) => {
      try {
        return (JSON.parse(key) as { bookId?: string }).bookId === bookId;
      } catch {
        return false;
      }
    };
    for (const [key, audio] of this.ready) {
      if (belongs(key)) {
        this.readyBytes -= audio.data.length * 2;
        this.ready.delete(key);
      }
    }
    for (const key of this.failed.keys())
      if (belongs(key)) this.failed.delete(key);
    for (const key of this.pending.keys())
      if (belongs(key)) this.invalidated.add(key);
  }
  async settle(): Promise<void> {
    await Promise.allSettled([...this.pending.values()]);
  }
  prefetch(
    key: string | undefined,
    playing: boolean,
    context?: PlaybackRequest,
  ): void {
    if (!key || !playing || this.prefetchPending) return;
    this.prefetchPending = true;
    void this.get(key, false, context)
      .catch(() => {})
      .finally(() => {
        this.prefetchPending = false;
      });
  }
}

export function blockForOffset(blocks: AudioBlock[], offset: number): number {
  const found = blocks.findIndex((block) => block.end > offset);
  return found < 0 ? Math.max(0, blocks.length - 1) : found;
}
export function blockOffsetFraction(block: AudioBlock, offset: number): number {
  const total = block.passages.reduce((sum, part) => sum + part.weight, 0);
  if (total <= 0) return 0;
  let elapsed = 0;
  for (const part of block.passages) {
    if (offset < part.end)
      return Math.max(
        0,
        Math.min(
          1,
          (elapsed +
            part.weight *
              Math.max(
                0,
                Math.min(
                  1,
                  (offset - part.start) / Math.max(1, part.end - part.start),
                ),
              )) /
            total,
        ),
      );
    elapsed += part.weight;
  }
  return 1;
}
export function blockPosition(
  block: AudioBlock,
  fraction: number,
): { index: number; offset: number } | null {
  const total = block.passages.reduce((sum, part) => sum + part.weight, 0);
  if (!block.passages.length) return null;
  let elapsed = Math.max(0, Math.min(1, fraction)) * total;
  for (const part of block.passages) {
    if (elapsed < part.weight)
      return {
        index: part.index,
        offset: Math.min(
          part.end,
          part.start +
            Math.floor(
              ((part.end - part.start) * elapsed) / Math.max(part.weight, 1),
            ),
        ),
      };
    elapsed -= part.weight;
  }
  const last = block.passages[block.passages.length - 1]!;
  return { index: last.index, offset: last.end };
}

export class PlaybackGate {
  private paused = false;
  private waiters: (() => void)[] = [];
  pause(): void {
    this.paused = true;
  }
  resume(): void {
    this.paused = false;
    this.wake();
  }
  cancel(): void {
    this.paused = false;
    this.wake();
  }
  private wake(): void {
    const waiters = this.waiters;
    this.waiters = [];
    waiters.forEach((wake) => wake());
  }
  async wait(isCurrent: () => boolean): Promise<boolean> {
    while (this.paused && isCurrent())
      await new Promise<void>((resolve) => this.waiters.push(resolve));
    return isCurrent();
  }
}
