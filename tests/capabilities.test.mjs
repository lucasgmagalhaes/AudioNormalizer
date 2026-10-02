import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const capabilities = JSON.parse(readFileSync("backend/capabilities/default.json", "utf8"));
const main = readFileSync("src/main.ts", "utf8");

// Tauri denies plugin calls without a matching permission, so a missing entry
// makes the button handler fail silently. Keep each API used in the UI listed.
const required = [
  { api: /\bconfirm\(/, permission: "dialog:allow-confirm" },
  { api: /\bopen\(/, permission: "dialog:allow-open" },
  { api: /\bcheck\(/, permission: "updater:default" },
];

for (const { api, permission } of required) {
  test(`capability ${permission} is granted for the API used in main.ts`, () => {
    assert.match(main, api, `main.ts no longer uses ${permission}; remove it from this test`);
    assert.ok(capabilities.permissions.includes(permission), `missing ${permission}`);
  });
}

const html = readFileSync("index.html", "utf8");
const runNormalize = main.slice(main.indexOf("async function runNormalize"), main.indexOf("async function reassess"));

test("normalize asks for confirmation in the app before replacing the file", () => {
  const ask = runNormalize.indexOf("await askReplace(");
  const busy = runNormalize.indexOf('state.busy = "normalize"');
  assert.ok(ask !== -1, "runNormalize must await askReplace");
  assert.ok(busy > ask, "the job must only start after the user approved");
  assert.doesNotMatch(runNormalize, /\bconfirm\(/, "the destructive step must not use the native dialog");
});

test("saving a copy skips the replace confirmation and sends the output mode to the engine", () => {
  assert.match(runNormalize, /let approved = output === "copy"/, "a copy must not need the destructive confirmation");
  assert.match(runNormalize, /api\.normalize\(.*\{ output \}\)/, "the chosen output mode must reach the backend");
  assert.match(html, /<select id="output"[\s\S]*value="replace"[\s\S]*value="copy"/, "both output modes must be selectable");
});

test("normalize reports confirmation dialog failures instead of failing silently", () => {
  const ask = runNormalize.indexOf("await askReplace(");
  const tryIndex = runNormalize.lastIndexOf("try {", ask);
  assert.ok(tryIndex !== -1, "askReplace must be wrapped in try/catch");
  assert.match(runNormalize.slice(ask, ask + 300), /catch \(err\)[\s\S]*showError/);
});

test("the confirmation dialog markup matches the ids main.ts uses", () => {
  assert.match(html, /<dialog id="confirm-dialog"/);
  for (const id of ["confirm-lead", "confirm-target", "confirm-ceiling", "confirm-gain", "confirm-ok", "confirm-cancel"]) {
    assert.match(html, new RegExp(`id="${id}"`), `index.html is missing #${id}`);
    assert.match(main, new RegExp(`byId(?:<[A-Za-z]+>)?\\("${id}"\\)`), `main.ts does not read #${id}`);
  }
});
