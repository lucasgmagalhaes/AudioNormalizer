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

test("normalize click handler reports confirm dialog failures", () => {
  const body = main.slice(main.indexOf("async function runNormalize"));
  const confirmCall = body.indexOf("await confirm(");
  const tryIndex = body.lastIndexOf("try {", confirmCall);
  assert.ok(tryIndex !== -1 && tryIndex < confirmCall, "confirm must be wrapped in try/catch");
  assert.match(body.slice(confirmCall, confirmCall + 500), /catch \(err\)[\s\S]*showError/);
});
