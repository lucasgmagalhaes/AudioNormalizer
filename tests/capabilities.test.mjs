import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const capabilities = JSON.parse(readFileSync("backend/capabilities/default.json", "utf8"));
const main = readFileSync("src/main.ts", "utf8");

// Tauri denies plugin calls without a matching permission, so a missing entry
// makes the button handler fail silently. Keep each API used in the UI listed.
const required = [
  { api: /\bopen\(/, permission: "dialog:allow-open" },
  { api: /\bcheck\(/, permission: "updater:default" },
  { api: /\bopenUrl\(/, permission: "opener:default" },
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
  assert.match(runNormalize, /api\.normalize\([\s\S]*?\boutput,[\s\S]*?\}\)/, "the chosen output mode must reach the backend");
  assert.match(html, /<select id="output"[\s\S]*value="replace"[\s\S]*value="copy"/, "both output modes must be selectable");
});

test("the chosen audio track reaches both analysis and normalization", () => {
  assert.match(html, /<select id="track"/, "the track selector must exist");
  assert.match(main, /api\.analyze\(file\.path, targets\(\), track\)/, "analysis must measure the chosen track");
  assert.match(runNormalize, /track: choice\.track,\s*allTracks: choice\.all/, "normalization must receive the track choice");
  assert.match(runNormalize, /state\.analyzedTrack === choice\.track/, "an analysis of another track must not be reused");
});

test("the custom target limits match the ones the engine enforces", () => {
  const engine = readFileSync("backend/src/engine/mod.rs", "utf8").match(/\((-?\d+)\.0\.\.=(-?\d+)\.0\)\.contains\(&self\.target_lufs\)/);
  assert.ok(engine, "could not find the target range in engine/mod.rs");
  const [min, max] = [Number(engine[1]), Number(engine[2])];
  assert.match(main, new RegExp(`TARGET_RANGE = \\{ min: ${min}, max: ${max} \\}`), "main.ts range differs from the engine");
  assert.match(html, new RegExp(`id="target-custom"[^>]*min="${min}"[^>]*max="${max}"`), "the input limits differ from the engine");
});

test("leveling and the result checks are wired end to end", () => {
  assert.match(html, /<input id="leveling" type="checkbox"/);
  assert.match(runNormalize, /leveling: ui\.leveling\.checked/, "the leveling choice must reach the engine");
  assert.match(main, /LOUDNESS_TOLERANCE_LU = 0\.5/, "EBU R128 tolerance is +-0.5 LU");
  assert.match(main, /report\.truePeakCeilingDb/, "the peak check needs the ceiling the engine used");
});

const runBatch = main.slice(main.indexOf("async function runBatch"), main.indexOf("async function checkForUpdates"));

test("a batch asks once before replacing and then runs the files one after another", () => {
  const ask = runBatch.indexOf("await askReplace(");
  const busy = runBatch.indexOf('state.busy = "normalize"');
  assert.ok(ask !== -1 && busy > ask, "the batch must only start after the user approved");
  assert.match(runBatch, /let approved = output === "copy"/, "copies need no confirmation");
  assert.match(runBatch, /for \(const \[index, item\] of items\.entries\(\)\)/, "files must be processed in order");
  assert.match(runBatch, /await api\.normalize\(item\.file\.path/, "each file is normalized on its own");
  assert.match(runBatch, /isCancelled\(err\)[\s\S]*state\.stopped = true/, "cancelling must stop the rest of the queue");
  assert.match(runBatch, /item\.status = "failed"/, "one failure must not stop the others");
});

test("several files can be chosen from the dialog and by dropping", () => {
  assert.match(main, /multiple: true/, "the file dialog must allow several files");
  assert.match(main, /void selectFiles\(payload\.paths\)/, "dropping several files must build a queue");
  assert.match(html, /<ol id="queue-list"/);
});

test("the lossless choice reaches the engine for single files and for the queue", () => {
  assert.match(html, /<input id="lossless" type="checkbox"/);
  assert.equal(main.match(/lossless: ui\.lossless\.checked/g)?.length, 2, "both normalize paths must send it");
});

test("the report can be copied and uses the measured values", () => {
  assert.match(html, /id="copy-report"/);
  assert.match(main, /navigator\.clipboard\.writeText\(reportText\(state\.report\)\)/);
  assert.match(main, /report\.inputMaxShortTermLufs/, "the loudest short-term reading must reach the report");
  assert.match(main, /catch \{[\s\S]*showError\(t\("copyFailed"\)\)/, "a failed copy must be reported");
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

test("the optional audio clean-up is off by default and reaches the engine for single files and the queue", () => {
  for (const id of ["highpass", "declip"]) {
    assert.match(html, new RegExp(`<input id="${id}" type="checkbox"(?![^>]*checked)`), `${id} must start unchecked`);
  }
  assert.equal(main.match(/cleanup: cleanupOptions\(\)/g)?.length, 2, "both normalize paths must send it");
  assert.match(main, /highpass: ui\.highpass\.checked, declip: ui\.declip\.checked/);
});

test("the app updates itself before showing anything, and the menu can do it again", () => {
  assert.match(html, /<div id="viewport" class="viewport" inert>/, "the app must start covered and unreachable");
  assert.match(html, /<div id="update-screen" class="update-screen"(?![^>]*hidden)/, "the update screen must be up from the first paint");
  assert.match(main, /check\(\{ timeout: UPDATE_CHECK_TIMEOUT_MS \}\)/, "no internet must not keep the app closed");
  assert.match(main, /update\.downloadAndInstall\(\(event\)/, "the download must report progress");
  assert.match(main, /if \(autoUpdateEnabled\(\)\) \{\s*void runUpdate\(false\);/, "the check runs at startup");
  assert.match(main, /run: \(\) => void runUpdate\(true\)/, "the Help menu must run the same update");
  assert.doesNotMatch(main, /confirm\(t\("update/, "no modal asks before updating");
});

test("no unused dialog permission stays granted", () => {
  for (const permission of ["dialog:allow-confirm", "dialog:allow-message"]) {
    assert.ok(!capabilities.permissions.includes(permission), `${permission} is not used by the UI`);
  }
});
