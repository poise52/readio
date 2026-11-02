import type { OfflineQuote, OfflineStatus } from "../types/models";

export function shouldPollOffline(
  statuses: OfflineStatus[],
  pausedDeadlines: ReadonlyMap<string, number>,
  now: number,
): boolean {
  return (
    statuses.some((status) => status.state === "running") ||
    [...pausedDeadlines.values()].some((deadline) => deadline > now)
  );
}

export function shouldRefreshPausedQuote(
  status: OfflineStatus | undefined,
  quote: OfflineQuote | null,
  dialogOpen: boolean,
  busy: boolean,
): boolean {
  return !!(
    dialogOpen &&
    !busy &&
    status?.state === "paused" &&
    quote &&
    status.cachedBlocks > quote.cachedBlocks
  );
}

export function isStaleQuoteError(error: unknown): boolean {
  return String(error).includes("запросите оценку снова");
}
