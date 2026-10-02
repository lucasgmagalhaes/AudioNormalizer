import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const languages = ["pt-BR", "en", "es"];
const kebab = (key) => key.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);

function messages(language) {
  const source = readFileSync(`src/locales/${language}.ftl`, "utf8");
  const entries = [...source.matchAll(/^([a-z][a-z0-9-]*) = (.+)$/gm)];
  return new Map(entries.map(([, id, value]) => [id, value.trimEnd()]));
}

const bundles = Object.fromEntries(languages.map((language) => [language, messages(language)]));
const html = readFileSync("index.html", "utf8");
const main = readFileSync("src/main.ts", "utf8");

const placeholders = (value) => [...value.matchAll(/\{ \$(\w+) \}/g)].map((m) => m[1]).sort();

function usedKeys() {
  const fromHtml = [...html.matchAll(/data-i18n(?:-aria-label)?="([^"]+)"/g)].map((m) => m[1]);
  const fromCode = [...main.matchAll(/\bt\("([A-Za-z]+)"/g)].map((m) => m[1]);
  // Keys composed at runtime: stage<Stage> and verdict<Verdict>.
  const dynamic = ["Analyze", "Calibrate", "Normalize", "Retry", "Verify", "Finalize"]
    .map((stage) => `stage${stage}`)
    .concat(["None", "Small", "Moderate", "Large"].map((verdict) => `verdict${verdict}`));
  return [...new Set([...fromHtml, ...fromCode, ...dynamic])];
}

test("every language defines the same message ids", () => {
  const reference = [...bundles["pt-BR"].keys()].sort();
  for (const language of languages) {
    assert.deepEqual([...bundles[language].keys()].sort(), reference, `${language} ids differ from pt-BR`);
  }
});

test("every language uses the same placeholders per message", () => {
  for (const [id, value] of bundles["pt-BR"]) {
    for (const language of languages) {
      assert.deepEqual(placeholders(bundles[language].get(id)), placeholders(value), `${language}/${id}`);
    }
  }
});

test("every key used by the HTML and main.ts exists in every language", () => {
  for (const key of usedKeys()) {
    for (const language of languages) {
      assert.ok(bundles[language].has(kebab(key)), `${language} is missing "${key}"`);
    }
  }
});

test("Portuguese text is not copied into English", () => {
  const language = bundles.en;
  for (const [id, value] of bundles["pt-BR"]) {
    // Pure numbers, units and loan words are legitimately identical.
    if (/^[-\d\s.,A-Za-z·/]+$/.test(value)) {
      continue;
    }
    assert.notEqual(language.get(id), value, `en/${id} is identical to pt-BR`);
  }
});
