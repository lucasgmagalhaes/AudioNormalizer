import assert from "node:assert/strict";
import test from "node:test";

// Node strips the types, so the TypeScript module is tested as it ships.
import { isFinished, settled, summarize } from "../src/batch.ts";

test("settled files are the ones that are over, whatever the outcome", () => {
  assert.equal(settled(["done", "failed", "skipped", "running", "waiting"]), 3);
  assert.equal(settled([]), 0);
});

test("the summary counts each outcome", () => {
  const summary = summarize(["done", "done", "failed", "skipped", "done"]);
  assert.deepEqual(summary, { total: 5, done: 3, failed: 1, skipped: 1 });
});

test("a batch is finished only when nothing is waiting or running", () => {
  assert.equal(isFinished(["done", "failed", "skipped"]), true);
  assert.equal(isFinished(["done", "running", "waiting"]), false);
  assert.equal(isFinished(["done", "waiting"]), false);
  assert.equal(isFinished([]), true);
});
