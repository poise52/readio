// Generated from crates/desk-app/src/ipc.rs; do not edit by hand.
// prettier-ignore
export type IpcErrorCode = "cancelled" | "retry_required" | "failed" | "stale_quote" | "budget_exceeded" | "invalid_request";
// prettier-ignore
export type OfflineJobState = "none" | "paused" | "running" | "complete" | "error";
// prettier-ignore
export type GenerationState = "prepared" | "submitted" | "completed" | "uncertain";
export interface IpcError {
  code: IpcErrorCode;
  message: string;
  retryable: boolean;
  details?: Record<string, string>;
  attemptId?: string;
  state?: GenerationState;
}
