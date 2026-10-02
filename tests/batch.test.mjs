import assert from "node:assert/strict";
import test from "node:test";

// Node strips the types, so the TypeScript module is tested as it ships.
import { isFinished, overallPercent, summarize } from "../src/batch.ts";

test("overall progress spreads the files evenly over the bar", () => {
  assert.equal(overallPercent(0, 4, 0), 0);
  assert.equal(overallPercent(0, 4, 100), 25);
  assert.equal(overallPercent(2, 4, 50), 62.5);
  assert.equal(overallPercent(3, 4, 100), 100);
});

test("overall progress never leaves the 0..100 range", () => {
  assert.equal(overallPercent(0, 0, 50), 0, "an empty batch has no progress");
  assert.equal(overallPercent(0, 2, -20), 0);
  assert.equal(overallPercent(1, 2, 250), 100);
});

test("a single file maps one to one", () => {
  assert.equal(overallPercent(0, 1, 37), 37);
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
