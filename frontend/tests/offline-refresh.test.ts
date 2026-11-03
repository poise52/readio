import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  shouldRefreshPausedQuote,
  isStaleQuoteError,
} from "../app/utils/offline-refresh.ts";
import type { OfflineStatus, OfflineQuote } from "../app/types/models.ts";

const paused = {
  bookId: "book",
  state: "paused",
  cachedBlocks: 1,
  totalBlocks: 2,
} as OfflineStatus;
const quote = { cachedBlocks: 0 } as OfflineQuote;

test("late cached block triggers a new quote only for an open, idle paused dialog", () => {
  assert.equal(shouldRefreshPausedQuote(paused, quote, true, false), true);
  assert.equal(shouldRefreshPausedQuote(paused, quote, false, false), false);
  assert.equal(shouldRefreshPausedQuote(paused, quote, true, true), false);
  assert.equal(
    shouldRefreshPausedQuote(
      paused,
      { ...quote, cachedBlocks: 1 },
      true,
      false,
    ),
    false,
  );
  assert.equal(
    isStaleQuoteError({ code: "stale_quote", message: "Localised text" }),
    true,
  );
  assert.equal(isStaleQuoteError("Сеть недоступна"), false);
});
