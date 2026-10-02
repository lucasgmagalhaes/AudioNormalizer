import { readFile, writeFile } from "node:fs/promises";

const version = process.argv[2];
if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  throw new Error("Version must be valid SemVer without a v prefix.");
}

async function json(path) {
  const value = JSON.parse(await readFile(path, "utf8"));
  value.version = version;
  if (value.packages?.[""]) value.packages[""].version = version;
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}

async function toml(path) {
  const value = await readFile(path, "utf8");
  await writeFile(path, value.replace(/^(version = )"[^"]+"$/m, `$1"${version}"`));
}

await Promise.all([
  json("package.json"),
  json("package-lock.json"),
  toml("backend/Cargo.toml"),
  json("backend/tauri.conf.json"),
]);
