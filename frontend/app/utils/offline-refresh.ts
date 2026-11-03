import type { OfflineQuote, OfflineStatus } from "../types/models";
import type { IpcErrorCode } from "../types/ipc";

const staleQuote: IpcErrorCode = "stale_quote";

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
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    error.code === staleQuote
  );
}
