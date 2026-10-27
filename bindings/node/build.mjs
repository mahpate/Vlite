import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const root = fileURLToPath(new URL("../..", import.meta.url));
function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} failed: ${result.status}`);
}
if (process.env.CARGO_BUILD_TARGET) throw new Error("Cross-compilation is not supported");
const target = join(root, "target", "node-embedded");
// Distributed packages include the opt-in property-graph commands (GATE-008, v0.5.0).
run("cargo", ["build", "--locked", "--release", "-p", "vlite-ffi", "--target-dir", target], root);
run(process.execPath, [join(here, "node_modules", "node-gyp", "bin", "node-gyp.js"), "rebuild"], here);
const name = process.platform === "win32" ? "vlite_ffi.dll" : process.platform === "darwin" ? "libvlite_ffi.dylib" : "libvlite_ffi.so";
const output = join(here, "prebuilds", `${process.platform}-${process.arch}`);
mkdirSync(output, { recursive: true });
copyFileSync(realpathSync(join(target, "release", name)), join(output, name));
copyFileSync(join(here, "build", "Release", "vlite_node.node"), join(output, "vlite_node.node"));
console.log(`Built host-native package at ${output}`);
