"""Run bounded hand-written KV tool mutants, restoring and hash-checking each source."""

import hashlib
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
KV = "src/bin/tool_support/kv_mcp.rs"
MCP = "src/bin/vlite-mcp.rs"
SUPPORT = "src/bin/tool_support/mod.rs"

# name, file, exact replacements, test target, test name
MUTANTS = [
    ("namespace-bound-off-by-one", KV, [
        ('bounded(namespace, "namespace", 1, 255)?;', 'bounded(namespace, "namespace", 1, 254)?;'),
    ], "kv_tools", "bounds_and_tool_listing"),
    ("writable-call-gate-dropped", KV, [
        ("if read_only && matches!(name, PUT | DELETE) {", "if false && read_only && matches!(name, PUT | DELETE) {"),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("writable-list-gate-dropped", KV, [
        ("if read_only && write {", "if false && read_only && write {"),
    ], "kv_tools", "bounds_and_tool_listing"),
    ("expected-absent-reversed", KV, [
        ("Self::Name(AnyAbsent::Absent) => Expected::Absent,", "Self::Name(AnyAbsent::Absent) => Expected::Any,"),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("base64-canonical-check-removed", KV, [
        ("if encode_base64(&out) != text {", "if false && encode_base64(&out) != text {"),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("decimal-json-integer-accepted", KV, [
        ("struct Revision {\n    revision: String,\n}", """struct Revision {
    #[serde(deserialize_with = "mutant_integer_decimal")]
    revision: String,
}
fn mutant_integer_decimal<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = Value::deserialize(d)?;
    Ok(value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()))
}"""),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("missing-key-mapped-to-success", KV, [
        ('.ok_or("kv_not_found: key is absent or expired")?;',
         '.unwrap_or(KvEntry {key:request.key.clone(),value:Vec::new(),version:1,expires_at_ms:None});'),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("implicit-collection-creation", KV, [
        ("            db.kv_view(&request.collection).map_err(failure)?;\n", ""),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("unknown-argument-key-accepted", KV, [
        ("#[serde(deny_unknown_fields)]\n", ""),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("shell-error-exit-zero", SUPPORT, [
        ("ExitCode::FAILURE", "ExitCode::SUCCESS"),
    ], "kv_tools", "shell_stops_at_first_error"),
    ("mcp-iserror-missing", MCP, [
        ('"isError":true', '"mutantError":true'),
    ], "kv_tools", "contract_goldens_mcp_and_shell"),
    ("duplicate-key-accepted", KV, [
        ("if !keys.insert(key) {", "if !keys.insert(key) && false {"),
    ], "kv_tools", "duplicate_fields_and_malformed_mcp_params"),
    ("response-byte-bound-off-by-one", KV, [
        ("if size > PAGE_BYTES {", "if size >= PAGE_BYTES {"),
    ], "vlite-mcp", "kv_mcp::tests::page_exact_encoded_boundary"),
]


def main():
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    failures = []
    for name, file, replacements, target, test in MUTANTS:
        path = ROOT / file
        original = path.read_bytes()
        digest = hashlib.sha256(original).hexdigest()
        mutated = original.decode("utf-8").replace("\r\n", "\n")
        for before, after in replacements:
            assert before in mutated, (name, before)
            mutated = mutated.replace(before, after)
        if b"\r\n" in original:
            mutated = mutated.replace("\n", "\r\n")
        try:
            path.write_bytes(mutated.encode("utf-8"))
            selector = "--bin" if target == "vlite-mcp" else "--test"
            command = ["cargo", "test", "--locked", "--all-features", selector,
                       target, test, "--", "--exact"]
            result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True,
                                    text=True, timeout=600)
            output = result.stdout + result.stderr
            killed = result.returncode != 0 and "test result: FAILED" in output
            print(f"{name}: {'KILLED' if killed else 'SURVIVED/INVALID'} by {test}", flush=True)
            if not killed:
                failures.append(name)
                print(output[-4000:], flush=True)
        finally:
            path.write_bytes(original)
            assert hashlib.sha256(path.read_bytes()).hexdigest() == digest
            print(f"  restored SHA256 {digest}", flush=True)
    if failures:
        raise SystemExit(f"Mutants not killed: {failures}")
    print(f"All {len(MUTANTS)} mutants killed and byte-for-byte restored.")


if __name__ == "__main__":
    main()
