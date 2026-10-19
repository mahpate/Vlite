import { mkdtempSync, mkdirSync, copyFileSync, cpSync, readFileSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const here = fileURLToPath(new URL(".", import.meta.url));
const output = resolve(process.argv[2] ?? ".");
const platform = `${process.platform}-${process.arch}`;
const staging = mkdtempSync(join(tmpdir(), "vlite-node-pack-"));
const npm = process.platform === "win32" ? process.execPath : "npm";
const npmArgs = process.platform === "win32"
  ? [process.env.npm_execpath ?? join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js")] : [];
try {
  const metadata = JSON.parse(readFileSync(join(here, "package.json")));
  delete metadata.devDependencies;
  delete metadata.private;
  // No compilation or native downloads when installing a prebuilt package.
  metadata.scripts = { install: 'node -e ""' };
  metadata.os = [process.platform];
  metadata.cpu = [process.arch];
  writeFileSync(join(staging, "package.json"), JSON.stringify(metadata, null, 2));
  for (const name of ["index.js", "index.d.ts", "worker.js", "LICENSE", "README.md"]) {
    copyFileSync(join(here, name), join(staging, name));
  }
  cpSync(join(here, "prebuilds", platform), join(staging, "prebuilds", platform), { recursive: true });
  mkdirSync(output, { recursive: true });
  const name = `vlite-embedded-${metadata.version}-${platform}.tgz`;
  if (existsSync(join(output, name)) || existsSync(join(output, name + ".sha256"))) throw new Error("Package output already exists");
  const result = spawnSync(npm, [...npmArgs, "pack", "--ignore-scripts", "--json", "--pack-destination", staging],
    { cwd: staging, encoding: "utf8" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(result.stderr);
  const packed = JSON.parse(result.stdout)[0];
  const bytes = readFileSync(join(staging, packed.filename));
  writeFileSync(join(output, name), bytes, { flag: "wx" });
  writeFileSync(join(output, name + ".sha256"),
    createHash("sha256").update(bytes).digest("hex") + "  " + name + "\n", { flag: "wx" });
  console.log(join(output, name));
} finally {
  rmSync(staging, { recursive: true });
}
