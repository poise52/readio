import { test } from "node:test";
import { strict as assert } from "node:assert";
import { PlaybackController } from "../app/utils/playback-controller.ts";
import type { BlockAudio } from "../app/utils/block-playback.ts";
import type { AudioBlock } from "../app/types/models.ts";
class AudioFake implements BlockAudio {
  currentTime = 0;
  seeking = false;
  duration = 30;
  playbackRate = 1;
  preservesPitch = false;
  volume = 1;
  src = "";
  onloadedmetadata: ((event: Event) => unknown) | null = null;
  onseeked: ((event: Event) => unknown) | null = null;
  ontimeupdate: ((event: Event) => unknown) | null = null;
  onended: ((event: Event) => unknown) | null = null;
  onerror: ((event: Event) => unknown) | null = null;
  async play() {}
  pause() {}
}
const block = {
  id: "a",
  chapter: 0,
  start: 0,
  end: 30,
  passages: [{ index: 0, start: 0, end: 30, weight: 30 }],
} as AudioBlock;
const deferred = <T>() => {
  let resolve!: (result: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
};

test("pause during IPC holds the actual response, resume starts audio at saved time and seek reports its new position", async () => {
  const response = deferred<string>();
  const audio = new AudioFake();
  let playing = false;
  let time = 6000;
  const positions: number[] = [];
  const controller = new PlaybackController({
    fetch: (_: string) => response.promise,
    discard: () => assert.fail(),
    createAudio: () => audio,
    now: () => time,
  });
  const token = controller.token;
  controller.pause();
  let loaded = false;
  const pending = controller
    .load("book/profile/source", token)
    .then((value) => {
      loaded = true;
      return value;
    });
  response.resolve("audio");
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(loaded, false);
  controller.resume();
  const source = await pending;
  playing = true;
  const session = controller.block(source, {
    block,
    startingOffset: 0,
    startingSeconds: 8.5,
    rate: 1,
    volume: 1,
    isCurrent: () => token === controller.token,
    isPlaying: () => playing,
    isStartCurrent: () => true,
    onTimingMode: () => {},
    onReady: () => {},
    onPosition: (_, seconds) => positions.push(seconds),
    onStarted: () => {},
  });
  session.start();
  audio.onloadedmetadata?.(new Event("loadedmetadata"));
  assert.equal(audio.currentTime, 8.5);
  playing = false;
  audio.currentTime = 18;
  audio.ontimeupdate?.(new Event("timeupdate"));
  assert.deepEqual(positions, []);
  playing = true;
  time += 5000;
  audio.ontimeupdate?.(new Event("timeupdate"));
  assert.deepEqual(positions, [18]);
  session.cancel();
  await session.finished;
});

test("source or key changes and shutdown discard late IPC responses without starting audio", async () => {
  const requests = [deferred<string>(), deferred<string>(), deferred<string>()];
  let index = 0;
  const discarded: string[] = [];
  let created = 0;
  const controller = new PlaybackController({
    fetch: (_: string) => requests[index++]!.promise,
    discard: (_, value) => discarded.push(value),
    createAudio: () => {
      created++;
      return new AudioFake();
    },
    now: () => 0,
  });
  for (const [i, reason] of ["source", "key", "shutdown"].entries()) {
    const pending = controller.load(reason, controller.token);
    controller.cancel();
    requests[i]!.resolve(reason);
    await assert.rejects(
      pending,
      (error: unknown) =>
        typeof error === "object" &&
        error !== null &&
        "code" in error &&
        error.code === "cancelled",
    );
  }
  assert.deepEqual(discarded, ["source", "key", "shutdown"]);
  assert.equal(created, 0);
});

test("reopened book retains its exact position through early timeupdates and an asynchronous seek", async () => {
  const { startingBlockSeconds } =
    await import("../app/utils/audio-position.ts");
  const book = {
    chapter: 0,
    offset: 0,
    audioPosition: {
      chapter: 0,
      blockId: "a",
      profileId: "profile",
      seconds: 3.305340111,
    },
  };
  const opened = structuredClone(book);
  let ready = false,
    currentTime = 0,
    target = 0;
  const player = new AudioFake();
  Object.defineProperty(player, "currentTime", {
    get: () => currentTime,
    set: (seconds: number) => {
      target = seconds;
      player.seeking = true;
    },
  });
  const controller = new PlaybackController({
    fetch: async (_: string) => ({ resourceId: "cached" }),
    discard: () => {},
    createAudio: () => player,
    now: () => 6000,
  });
  const token = controller.token;
  const audio = await controller.load("cached block", token);
  const session = controller.block(audio.resourceId, {
    block,
    startingOffset: opened.offset,
    startingSeconds: startingBlockSeconds(
      opened.audioPosition,
      opened.chapter,
      block.id,
      "profile",
    ),
    rate: 1,
    volume: 1,
    isCurrent: () => token === controller.token,
    isPlaying: () => true,
    isStartCurrent: () => true,
    onTimingMode: () => {},
    onReady: () => {
      ready = true;
    },
    onPosition: (offset, seconds) => {
      opened.offset = offset;
      opened.audioPosition.seconds = seconds;
    },
    onStarted: () => {},
  });
  session.start();
  player.ontimeupdate?.(new Event("timeupdate"));
  assert.equal(opened.audioPosition.seconds, 3.305340111);
  player.onloadedmetadata?.(new Event("loadedmetadata"));
  assert.equal(target, 3.305340111);
  assert.equal(ready, false);
  player.ontimeupdate?.(new Event("timeupdate"));
  assert.equal(opened.audioPosition.seconds, 3.305340111);
  currentTime = target;
  player.seeking = false;
  player.onseeked?.(new Event("seeked"));
  assert.equal(ready, true);
  currentTime += 0.5;
  player.ontimeupdate?.(new Event("timeupdate"));
  assert.equal(opened.audioPosition.seconds, 3.805340111);
  session.cancel();
  await session.finished;
});
