// Pure helpers for the file queue, kept free of the DOM so they can be tested.

export type QueueStatus = "waiting" | "running" | "done" | "failed" | "skipped";

export interface BatchSummary {
  total: number;
  done: number;
  failed: number;
  skipped: number;
}

/** How many files are over, whatever the outcome. */
export function settled(statuses: readonly QueueStatus[]): number {
  return statuses.filter((status) => status !== "waiting" && status !== "running").length;
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
