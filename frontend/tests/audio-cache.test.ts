import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  AudioBlockLoader,
  PlaybackGate,
  blockForOffset,
  blockOffsetFraction,
  blockPosition,
} from "../app/utils/audio-cache.ts";
import type { AudioBlock, SpeechAudio } from "../app/types/models.ts";

test("pause during synthesis retains one request and resumes the same result", async () => {
  let calls = 0,
    release!: (audio: SpeechAudio) => void,
    played = false;
  const pending = new Promise<SpeechAudio>((resolve) => {
    release = resolve;
  });
  const loader = new AudioBlockLoader(async () => {
    calls++;
    return pending;
  });
  const gate = new PlaybackGate();
  const run = (async () => {
    const audio = await loader.get("chapter:block");
    if (await gate.wait(() => true)) played = !!audio;
  })();
  gate.pause();
  release({ data: "cached", mime: "audio/wav" });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(played, false);
  gate.resume();
  await run;
  assert.equal(played, true);
  assert.equal(calls, 1);
});

test("same block shares in-flight work and prefetch does not start while paused", async () => {
  let calls = 0,
    release!: (audio: SpeechAudio) => void;
  const pending = new Promise<SpeechAudio>((resolve) => {
    release = resolve;
  });
  const loader = new AudioBlockLoader(async () => {
    calls++;
    return pending;
  });
  loader.prefetch("next", false);
  assert.equal(calls, 0);
  const first = loader.get("same");
  const siblingSeek = loader.get("same");
  assert.equal(calls, 1);
  release({ data: "same", mime: "audio/wav" });
  assert.deepEqual(await first, await siblingSeek);
  await loader.get("same");
  assert.equal(calls, 1);
});

test("failed fetch is not cached and can be retried", async () => {
  let calls = 0;
  const loader = new AudioBlockLoader(async () => {
    if (++calls === 1) throw new Error("offline");
    return { data: "ok", mime: "audio/wav" };
  });
  await assert.rejects(loader.get("block"));
  await assert.rejects(loader.get("block"));
  assert.equal((await loader.get("block", true)).data, "ok");
  assert.equal(calls, 2);
});

test("offset seeks use canonical block and preserve split-passage offsets", () => {
  const blocks: AudioBlock[] = [
    {
      id: "a",
      chapter: 0,
      start: 0,
      end: 10,
      text: "",
      characters: 10,
      passages: [{ index: 0, start: 0, end: 10, weight: 10 }],
    },
    {
      id: "b",
      chapter: 0,
      start: 10,
      end: 30,
      text: "",
      characters: 20,
      passages: [
        { index: 0, start: 10, end: 20, weight: 10 },
        { index: 1, start: 20, end: 30, weight: 10 },
      ],
    },
  ];
  assert.equal(blockForOffset(blocks, 15), 1);
  assert.equal(blockOffsetFraction(blocks[1]!, 15), 0.25);
  assert.deepEqual(blockPosition(blocks[1]!, 0.75), { index: 1, offset: 25 });
});

test("removing a book clears memory and does not retain an in-flight result", async () => {
  let calls = 0,
    release!: (audio: SpeechAudio) => void;
  const pending = new Promise<SpeechAudio>((resolve) => {
    release = resolve;
  });
  const loader = new AudioBlockLoader(async () => {
    calls++;
    return calls === 1 ? pending : { data: "new", mime: "audio/wav" };
  });
  const key = JSON.stringify({ bookId: "book", blockId: "a" });
  const first = loader.get(key);
  loader.clearBook("book");
  release({ data: "old", mime: "audio/wav" });
  await first;
  assert.equal((await loader.get(key)).data, "new");
  assert.equal(calls, 2);
});

test("failed prefetch is surfaced without retry until explicit play", async () => {
  let calls = 0;
  const loader = new AudioBlockLoader(async (_key, retry) => {
    calls++;
    if (!retry) throw new Error("ambiguous network failure");
    return { data: "ok", mime: "audio/wav" };
  });
  loader.prefetch("paid-next", true);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await assert.rejects(loader.get("paid-next"));
  assert.equal(calls, 1);
  assert.equal((await loader.get("paid-next", true)).data, "ok");
  assert.equal(calls, 2);
});

test("prefetch limits background work to one in-flight block", async () => {
  const requested: string[] = [];
  let release!: (audio: SpeechAudio) => void;
  const pending = new Promise<SpeechAudio>((resolve) => {
    release = resolve;
  });
  const loader = new AudioBlockLoader(async (key) => {
    requested.push(key);
    return pending;
  });
  loader.prefetch("next", true);
  loader.prefetch("later", true);
  assert.deepEqual(requested, ["next"]);
  release({ data: "ok", mime: "audio/wav" });
  await pending;
});

test("large audio is not retained in memory cache", async () => {
  let calls = 0;
  const large = "a".repeat(8 * 1024 * 1024 + 1);
  const loader = new AudioBlockLoader(async () => {
    calls++;
    return { data: large, mime: "audio/wav" };
  });
  await loader.get("large");
  await loader.get("large");
  assert.equal(calls, 2);
});

test("removing a book also clears its remembered failure", async () => {
  let calls = 0;
  const key = JSON.stringify({ bookId: "book", blockId: "a" });
  const loader = new AudioBlockLoader(async () => {
    if (++calls === 1) throw new Error("failed");
    return { data: "ok", mime: "audio/wav" };
  });
  await assert.rejects(loader.get(key));
  loader.clearBook("book");
  assert.equal((await loader.get(key)).data, "ok");
});

test("cancelled work is not remembered and can be requested after resume", async () => {
  let calls = 0;
  const loader = new AudioBlockLoader(async () => {
    if (++calls === 1) throw { code: "cancelled", message: "cancelled" };
    return { data: "resumed", mime: "audio/wav" };
  });
  await assert.rejects(loader.get("block"));
  assert.equal((await loader.get("block")).data, "resumed");
  assert.equal(calls, 2);
});

test("retry confirmation preserves error metadata and passes the exact attempt", async () => {
  const error = {
    code: "retry_required",
    message: "unknown result",
    attemptId: "attempt-1",
    state: "uncertain",
  };
  let calls = 0;
  const loader = new AudioBlockLoader(async (_key, _retry, context) => {
    if (++calls === 1) throw error;
    assert.equal(context?.confirmedAttempt, "attempt-1");
    return { data: "confirmed", mime: "audio/wav" };
  });
  await assert.rejects(loader.get("block"), (value) => value === error);
  await assert.rejects(loader.get("block"), (value) => value === error);
  assert.equal(
    (
      await loader.get("block", true, {
        session: "session",
        revision: 1,
        confirmedAttempt: error.attemptId,
      })
    ).data,
    "confirmed",
  );
  assert.equal(calls, 2);
});

test("prefetched resource is released on eviction but active resource is retained until playback finishes", async () => {
  const released: string[] = [];
  const loader = new AudioBlockLoader(
    async (key) => ({ resourceId: key, mime: "audio/mpeg" }),
    1,
    (audio) => {
      if (audio.resourceId) released.push(audio.resourceId);
    },
  );
  await loader.get("first");
  await loader.get("second");
  assert.deepEqual(released, ["first"]);
  loader.takeResource("second");
  await loader.get("third");
  assert.deepEqual(released, ["first"]);
});

test("reopening an invalidated pending block waits for disposal then fetches a new lease", async () => {
  let resolve!: (value: { resourceId: string; mime: string }) => void;
  let calls = 0;
  const released: string[] = [];
  const loader = new AudioBlockLoader(
    () =>
      ++calls === 1
        ? new Promise((r) => {
            resolve = r;
          })
        : Promise.resolve({ resourceId: "new", mime: "audio/mpeg" }),
    3,
    (audio) => {
      if (audio.resourceId) released.push(audio.resourceId);
    },
  );
  const key = JSON.stringify({ bookId: "book" });
  const stale = loader.get(key);
  loader.clearBook("book");
  const fresh = loader.get(key);
  resolve({ resourceId: "old", mime: "audio/mpeg" });
  await stale;
  assert.equal((await fresh).resourceId, "new");
  assert.deepEqual(released, ["old"]);
  assert.equal(calls, 2);
});
