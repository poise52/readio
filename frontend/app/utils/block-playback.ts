import type { AudioBlock, SpeechTiming } from "../types/models";
import { blockOffsetFraction, blockPosition } from "./audio-cache.ts";
import { configurePlaybackRate } from "./playback.ts";
import {
  timedParts,
  timedPosition,
  timedSeek,
  type TimedPart,
} from "./audio-timing.ts";

export interface BlockAudio {
  currentTime: number;
  seeking: boolean;
  duration: number;
  playbackRate: number;
  preservesPitch: boolean;
  volume: number;
  src: string;
  onloadedmetadata: ((event: Event) => unknown) | null;
  onseeked: ((event: Event) => unknown) | null;
  ontimeupdate: ((event: Event) => unknown) | null;
  onended: ((event: Event) => unknown) | null;
  onerror: ((event: Event) => unknown) | null;
  play(): Promise<void>;
  pause(): void;
}

export interface BlockPlaybackOptions<T extends BlockAudio> {
  createAudio: () => T;
  now: () => number;
  block: AudioBlock;
  startingOffset: number;
  startingSeconds?: number;
  timings?: SpeechTiming[] | null;
  onTimingMode: (parts: TimedPart[] | null) => void;
  rate: number;
  volume: number;
  isCurrent: () => boolean;
  isPlaying: () => boolean;
  isStartCurrent: () => boolean;
  onPosition: (offset: number, seconds: number, shouldSave: boolean) => void;
  onReady: () => void;
  onStarted: () => void;
}

export function playAudioBlock<T extends BlockAudio>(
  options: BlockPlaybackOptions<T>,
) {
  const player = options.createAudio();
  configurePlaybackRate(player, options.rate);
  player.volume = Math.min(1, Math.max(0, options.volume));
  let settled = false;
  let lastSaved = 0;
  let metadataLoaded = false;
  let ready = false;
  let parts: TimedPart[] | null = null;
  let finish!: (error?: Error) => void;
  const finished = new Promise<void>((resolve, reject) => {
    finish = (error) => {
      if (settled) return;
      settled = true;
      player.ontimeupdate = null;
      player.onloadedmetadata = null;
      player.onseeked = null;
      player.onended = null;
      player.onerror = null;
      if (error) reject(error);
      else resolve();
    };
    const markReady = () => {
      if (ready || !options.isCurrent()) return;
      ready = true;
      options.onReady();
    };
    player.onseeked = () => {
      if (metadataLoaded && !player.seeking) markReady();
    };
    player.onloadedmetadata = () => {
      if (
        metadataLoaded ||
        !options.isCurrent() ||
        !Number.isFinite(player.duration) ||
        player.duration <= 0
      )
        return;
      metadataLoaded = true;
      parts = timedParts(options.block, options.timings, player.duration);
      options.onTimingMode(parts);
      player.currentTime =
        options.startingSeconds === undefined
          ? ((parts && timedSeek(parts, options.startingOffset)) ??
            blockOffsetFraction(options.block, options.startingOffset) *
              player.duration)
          : Math.max(0, Math.min(options.startingSeconds, player.duration));
      if (!player.seeking) markReady();
    };
    player.ontimeupdate = () => {
      if (
        !ready ||
        !options.isCurrent() ||
        !options.isPlaying() ||
        !Number.isFinite(player.duration) ||
        player.duration <= 0
      )
        return;
      const position = blockPosition(
        options.block,
        player.currentTime / player.duration,
      );
      if (!position) return;
      const now = options.now();
      const shouldSave = now - lastSaved >= 5000;
      if (shouldSave) lastSaved = now;
      options.onPosition(
        parts
          ? (timedPosition(parts, player.currentTime) ?? position.offset)
          : position.offset,
        player.currentTime,
        shouldSave,
      );
    };
    player.onended = () => finish();
    player.onerror = () => finish(new Error("Не удалось воспроизвести аудио"));
  });
  return {
    player,
    finished,
    cancel: () => finish(),
    fail: (error: Error) => finish(error),
    start: () => {
      void player
        .play()
        .then(() => {
          if (
            !settled &&
            options.isCurrent() &&
            options.isPlaying() &&
            options.isStartCurrent()
          )
            options.onStarted();
        })
        .catch((error: unknown) => {
          if (
            !settled &&
            options.isCurrent() &&
            options.isPlaying() &&
            options.isStartCurrent()
          )
            finish(error instanceof Error ? error : new Error(String(error)));
        });
    },
  };
}
