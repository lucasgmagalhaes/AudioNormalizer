// Pure helpers for the file queue, kept free of the DOM so they can be tested.

export type QueueStatus = "waiting" | "running" | "done" | "failed" | "skipped";

export interface BatchSummary {
  total: number;
  done: number;
  failed: number;
  skipped: number;
}

/** Progress of the whole batch while file `index` (0-based) is at `filePercent`. */
export function overallPercent(index: number, total: number, filePercent: number): number {
  if (total <= 0) {
    return 0;
  }
  const clamped = Math.max(0, Math.min(100, filePercent));
  return Math.min(100, ((index + clamped / 100) / total) * 100);
}

export function summarize(statuses: readonly QueueStatus[]): BatchSummary {
  const count = (status: QueueStatus) => statuses.filter((s) => s === status).length;
  return {
    total: statuses.length,
    done: count("done"),
    failed: count("failed"),
    skipped: count("skipped"),
  };
}

/** The batch is over when no file is waiting or running. */
export function isFinished(statuses: readonly QueueStatus[]): boolean {
  return statuses.every((status) => status !== "waiting" && status !== "running");
}
