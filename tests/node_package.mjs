import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync, readFileSync, readdirSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const tarball = resolve(process.argv[2]);
const checksum = readFileSync(tarball + ".sha256", "utf8").split(" ")[0];
assert.equal(createHash("sha256").update(readFileSync(tarball)).digest("hex"), checksum);
const directory = mkdtempSync(join(tmpdir(), "vlite-node-install-"));
try {
  writeFileSync(join(directory, "package.json"), '{"type":"module","private":true}');
  const npm = process.platform === "win32" ? process.execPath : "npm";
  const npmArgs = process.platform === "win32"
    ? [process.env.npm_execpath ?? join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js")] : [];
  const install = spawnSync(npm, [...npmArgs, "install", "--ignore-scripts", "--offline", "--no-audit", "--no-fund", tarball],
    { cwd: directory, encoding: "utf8", timeout: 120000 });
  if (install.error) throw install.error;
  assert.equal(install.status, 0, install.stderr);
  const metadata = JSON.parse(readFileSync(join(directory, "node_modules", "@vlite", "embedded", "package.json")));
  assert.deepEqual(metadata.os, [process.platform]);
  assert.deepEqual(metadata.cpu, [process.arch]);
  assert.equal(metadata.dependencies, undefined);
  assert.equal(metadata.devDependencies, undefined);
  const installed = join(directory, "node_modules", "@vlite", "embedded");
  for (const name of ["LICENSE", "index.d.ts", "worker.js"]) assert.ok(existsSync(join(installed, name)), name);
  assert.deepEqual(readdirSync(join(installed, "prebuilds")), [`${process.platform}-${process.arch}`]);
  const code = `
import assert from "node:assert/strict";
import { NativeLibrary } from "@vlite/embedded";
const native = new NativeLibrary();
try {
  const db = await native.create("installed.vlite", 2);
  await db.put("18446744073709551615", [1,0], {payload: Buffer.from([0,255])});
  assert.equal((await db.get("18446744073709551615")).payload_base64,"AP8=");
  assert.equal((await db.search([1,0],1))[0].distance,0);
  await db.close();
  const next = await native.open("installed.vlite");
  assert.equal((await next.info()).records,1);
} finally { await native.close(); }
console.log("PASS offline installed embedded Node package");
`;
  const env = { ...process.env };
  delete env.NODE_PATH;
  delete env.NODE_OPTIONS;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", code],
    { cwd: directory, env, encoding: "utf8", timeout: 60000 });
  if (run.error) throw run.error;
  assert.equal(run.status, 0, run.stderr);
  console.log(run.stdout.trim());
} finally {
  rmSync(directory, { recursive: true });
}
