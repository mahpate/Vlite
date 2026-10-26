import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync, readFileSync, readdirSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const tarball = resolve(process.argv[2]);
const rest = process.argv[3] === "--rest";
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
  const packageName = rest ? "client" : "embedded";
  const metadata = JSON.parse(readFileSync(join(directory, "node_modules", "@vlite", packageName, "package.json")));
  if (!rest) {
    assert.deepEqual(metadata.os, [process.platform]);
    assert.deepEqual(metadata.cpu, [process.arch]);
  }
  assert.equal(metadata.dependencies, undefined);
  if (!rest) assert.equal(metadata.devDependencies, undefined);
  const installed = join(directory, "node_modules", "@vlite", packageName);
  for (const name of ["LICENSE", "index.d.ts", "index.js"]) assert.ok(existsSync(join(installed, name)), name);
  if (!rest) assert.deepEqual(readdirSync(join(installed, "prebuilds")), [`${process.platform}-${process.arch}`]);
  const code = rest ? `
import assert from "node:assert/strict";
import { Client, VLiteError } from "@vlite/client";
let called = false;
globalThis.fetch = async (url, options) => {
  assert.equal(url, "http://127.0.0.1:8080/v1/vectors/18446744073709551615");
  assert.equal(options.method, "GET");
  called = true;
  return new Response(JSON.stringify({id:"18446744073709551615",vector:[1,0],payload_base64:"",metadata:{}}),
    {headers:{"Content-Type":"application/json"}});
};
const record = await new Client("http://127.0.0.1:8080", "a".repeat(32)).get("18446744073709551615");
assert.equal(record.id,"18446744073709551615");
assert.ok(called);
assert.equal(typeof VLiteError,"function");
console.log("PASS offline installed REST Node package");
` : `
import assert from "node:assert/strict";
import { NativeLibrary } from "@vlite/embedded";
const native = new NativeLibrary();
try {
  // Release packages ship the opt-in graph commands (GATE-008, v0.5.0).
  assert.deepEqual(await native.capabilities(), {graph:true});
  const graphDb = await native.createWithGraph("graph.vlite", 2);
  assert.deepEqual(await graphDb.createGraph("g"), {name:"g",node_count:0,edge_count:0});
  await graphDb.close();
  const db = await native.create("installed.vlite", 2);
  await db.put("18446744073709551615", [1,0], {payload: Buffer.from([0,255])});
  assert.equal((await db.get("18446744073709551615")).payload_base64,"AP8=");
  assert.equal((await db.search([1,0],1))[0].distance,0);
  assert.equal((await db.search([1,0],1,{metric:"cosine"}))[0].distance,0);
  assert.equal((await db.search([1,0],1,{metric:"inner_product"}))[0].distance,-1);
  await db.buildIndex({metric:"cosine"});
  assert.equal((await db.searchApproximateReady([1,0],1,{metric:"cosine"}))[0].distance,0);
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
