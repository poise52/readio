import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  playAudioBlock,
  type BlockAudio,
} from "../app/utils/block-playback.ts";
import type { AudioBlock } from "../app/types/models.ts";

class FakeAudio implements BlockAudio {
  currentTime = 0;
  seeking = false;
  duration = 20;
  playbackRate = 1;
  preservesPitch = false;
  volume = 1;
  src = "";
  onloadedmetadata: ((event: Event) => unknown) | null = null;
  onseeked: ((event: Event) => unknown) | null = null;
  ontimeupdate: ((event: Event) => unknown) | null = null;
  onended: ((event: Event) => unknown) | null = null;
  onerror: ((event: Event) => unknown) | null = null;
  play = async () => {};
  pause() {}
}
const block = {
  id: "block",
  chapter: 0,
  start: 0,
  end: 20,
  passages: [{ index: 0, start: 0, end: 20, weight: 20 }],
} as AudioBlock;

test("restores exact audio time, pauses updates, seeks, and cancels stale events", async () => {
  const audio = new FakeAudio();
  let active = true,
    playing = true,
    clock = 6000;
  const positions: number[] = [];
  const session = playAudioBlock({
    createAudio: () => audio,
    now: () => clock,
    block,
    startingOffset: 3,
    startingSeconds: 7.25,
    rate: 1,
    volume: 1,
    isCurrent: () => active,
    isPlaying: () => playing,
    isStartCurrent: () => true,
    onTimingMode: () => {},
    onReady: () => {},
    onPosition: (_, seconds) => positions.push(seconds),
    onStarted: () => {},
  });
  session.start();
  audio.onloadedmetadata?.(new Event("loadedmetadata"));
  assert.equal(audio.currentTime, 7.25);
  audio.ontimeupdate?.(new Event("timeupdate"));
  playing = false;
  audio.currentTime = 12;
  audio.ontimeupdate?.(new Event("timeupdate"));
  playing = true;
  clock += 5000;
  audio.ontimeupdate?.(new Event("timeupdate"));
  assert.deepEqual(positions, [7.25, 12]);
  const staleUpdate = audio.ontimeupdate;
  active = false;
  session.cancel();
  await session.finished;
  staleUpdate?.(new Event("timeupdate"));
  assert.deepEqual(positions, [7.25, 12]);
  assert.equal(audio.ontimeupdate, null);
});

test("late play rejection after pause and resume cannot terminate the active session", async () => {
  const audio = new FakeAudio();
  let rejectPlay!: (reason: Error) => void;
  audio.play = () =>
    new Promise<void>((_, reject) => {
      rejectPlay = reject;
    });
  let attemptCurrent = true;
  const session = playAudioBlock({
    createAudio: () => audio,
    now: () => 0,
    block,
    startingOffset: 0,
    rate: 1,
    volume: 1,
    isCurrent: () => true,
    isPlaying: () => true,
    isStartCurrent: () => attemptCurrent,
    onTimingMode: () => {},
    onReady: () => {},
    onPosition: () => {},
    onStarted: () => {},
  });
  session.start();
  attemptCurrent = false;
  rejectPlay(new Error("late"));
  await Promise.resolve();
  await Promise.resolve();
  audio.onended?.(new Event("ended"));
  await session.finished;
});

test("provider word boundaries drive seek and highlight despite uneven pauses and dictionary expansion", async () => {
  const audio = new FakeAudio();
  audio.duration = 20;
  const expanded = {
    ...block,
    end: 12,
    passages: [
      {
        index: 0,
        start: 0,
        end: 3,
        weight: 100,
        synthesisStart: 0,
        synthesisEnd: 17,
      },
      {
        index: 1,
        start: 4,
        end: 12,
        weight: 1,
        synthesisStart: 19,
        synthesisEnd: 25,
      },
    ],
  } as AudioBlock;
  const offsets: number[] = [];
  let exact = false;
  const session = playAudioBlock({
    createAudio: () => audio,
    now: () => 6000,
    block: expanded,
    startingOffset: 4,
    timings: [
      { startUtf16: 0, endUtf16: 17, startSeconds: 0.2, endSeconds: 3 },
      { startUtf16: 19, endUtf16: 25, startSeconds: 11, endSeconds: 14 },
    ],
    onTimingMode: (parts) => {
      exact = !!parts;
    },
    rate: 1,
    volume: 1,
    isCurrent: () => true,
    isPlaying: () => true,
    isStartCurrent: () => true,
    onReady: () => {},
    onPosition: (offset) => offsets.push(offset),
    onStarted: () => {},
  });
  audio.onloadedmetadata?.(new Event("loadedmetadata"));
  assert.equal(exact, true);
  assert.equal(audio.currentTime, 11);
  audio.currentTime = 6;
  audio.ontimeupdate?.(new Event("timeupdate"));
  assert.equal(offsets.at(-1), 0);
  audio.currentTime = 11;
  audio.ontimeupdate?.(new Event("timeupdate"));
  assert.equal(offsets.at(-1), 4);
  session.cancel();
  await session.finished;
});

test("invalid or legacy timing metadata falls back to weighted positioning", async () => {
  const audio = new FakeAudio();
  let exact = true;
  const legacy = {
    ...block,
    passages: [{ ...block.passages[0], synthesisStart: 0, synthesisEnd: 4 }],
  } as AudioBlock;
  const session = playAudioBlock({
    createAudio: () => audio,
    now: () => 0,
    block: legacy,
    startingOffset: 10,
    timings: [{ startUtf16: 0, endUtf16: 5, startSeconds: 0, endSeconds: 1 }],
    onTimingMode: (parts) => {
      exact = !!parts;
    },
    rate: 1,
    volume: 1,
    isCurrent: () => true,
    isPlaying: () => true,
    isStartCurrent: () => true,
    onReady: () => {},
    onPosition: () => {},
    onStarted: () => {},
  });
  audio.onloadedmetadata?.(new Event("loadedmetadata"));
  assert.equal(exact, false);
  assert.equal(audio.currentTime, 10);
  session.cancel();
  await session.finished;
});
