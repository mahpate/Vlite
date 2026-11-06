#!/usr/bin/env python3
"""Prove that the CI test runners cover exactly the tests they replaced (standard library only).

  python scripts/ci_test_proof.py shards N -- CARGO_ARGS...
      Lists the selection with `cargo nextest list` once without a partition and once per hash
      bucket `hash:i/N`, and checks that the buckets are disjoint and together hold exactly the
      tests of the unpartitioned list. The Windows suite runs as N buckets on N runners; this is
      the proof that no test is lost or run twice.

  python scripts/ci_test_proof.py parity -- CARGO_ARGS...
      Lists the selection with `cargo test CARGO_ARGS -- --list` and with `cargo nextest list`, and
      checks that every test binary carries the same test names in both. Linux and macOS moved from
      `cargo test` to nextest; this is the proof that the same tests run (ignored ones are listed
      by both and skipped by both). Doc tests are not part of either list: `cargo test --doc` runs them.

CARGO_ARGS are the cargo arguments of the suite, for example
`--locked --workspace --all-targets --all-features`. Both modes list from a finished build, so run
them after the suite. They print short lines, append a table to $GITHUB_STEP_SUMMARY when it is set,
and exit 1 on any difference.
"""
import json
import os
import re
import subprocess
import sys

RUNNING = re.compile(r"^\s*Running .*\((?P<path>[^()]+)\)\s*$")
LISTED = re.compile(r"^(?P<name>.+): (?:test|benchmark)$")


def binary_key(path):
    """The file name of a test binary without its extension: unique, as it carries the target hash."""
    return re.split(r"[\\/]", path.strip())[-1].removesuffix(".exe")


def read_nextest(document):
    """{binary file name: {test name: (ignored, selected)}} from `cargo nextest list --message-format json`.

    `selected` is true when the filters and the partition of that listing choose the test.
    """
    found = {}
    for suite in json.loads(document)["rust-suites"].values():
        found[binary_key(suite["binary-path"])] = {
            name: (bool(case.get("ignored")), case.get("filter-match", {}).get("status") == "matches")
            for name, case in suite.get("testcases", {}).items()
        }
    return found


def names(listing, selected_only=False):
    """The set of (binary, test name) of a nextest listing."""
    return {
        (binary, name)
        for binary, tests in listing.items()
        for name, (_, selected) in tests.items()
        if selected or not selected_only
    }


def read_cargo_list(output):
    """The set of (binary file name, test name) in the interleaved output of `cargo test -- --list`."""
    found, current = set(), None
    for line in output.splitlines():
        header = RUNNING.match(line)
        if header:
            current = binary_key(header["path"])
            continue
        listed = LISTED.match(line)
        if listed and current is not None:
            found.add((current, listed["name"]))
    return found


def nextest(args, partition=None):
    command = ["cargo", "nextest", "list", "--message-format", "json", *args]
    if partition:
        command += ["--partition", partition]
    done = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if done.returncode != 0:
        sys.exit(f"{' '.join(command)} failed:\n{done.stderr[-2000:]}")
    return read_nextest(done.stdout)


def compare_buckets(whole, buckets):
    """Problems (empty when the buckets partition the selected tests of `whole`) and the bucket sizes."""
    selected = names(whole, selected_only=True)
    problems, sizes, union = [], [], set()
    for index, bucket in enumerate(buckets, 1):
        members = names(bucket, selected_only=True)
        sizes.append(len(members))
        repeated = union & members
        if repeated:
            problems.append(f"bucket {index} repeats {len(repeated)} tests, e.g. {sorted(repeated)[0]}")
        union |= members
    if union != selected:
        problems.append(
            f"the buckets hold {len(union)} tests, the unpartitioned list {len(selected)}; "
            f"missing e.g. {sorted(selected - union)[:1]}, extra e.g. {sorted(union - selected)[:1]}"
        )
    return problems, sizes


def compare_parity(cargo, listing):
    """Problems (empty when both runners list the same names in every binary)."""
    nextest_names = names(listing)
    problems = []
    for binary in sorted({b for b, _ in cargo} | {b for b, _ in nextest_names}):
        here = {n for b, n in cargo if b == binary}
        there = {n for b, n in nextest_names if b == binary}
        if here != there:
            problems.append(
                f"{binary}: only in cargo test {sorted(here - there)[:2]}, only in nextest {sorted(there - here)[:2]}"
            )
    return problems


def summarize(title, rows, problems):
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    with open(summary, "a", encoding="utf-8") as handle:
        handle.write(f"### {title}: {'identical' if not problems else 'DIFFERENT'}\n\n| what | tests |\n|---|---|\n")
        handle.writelines(f"| {what} | {count} |\n" for what, count in rows)
        handle.writelines(f"\n- {problem}\n" for problem in problems[:10])
        handle.write("\n")


def shards(count, args):
    whole = nextest(args)
    buckets = [nextest(args, f"hash:{i}/{count}") for i in range(1, count + 1)]
    problems, sizes = compare_buckets(whole, buckets)
    total = len(names(whole, selected_only=True))
    ignored = sum(1 for tests in whole.values() for flag, _ in tests.values() if flag)
    rows = [("unpartitioned, to run", total)] + [(f"bucket {i}/{count}", n) for i, n in enumerate(sizes, 1)]
    rows.append(("sum of the buckets", sum(sizes)))
    print(f"unpartitioned: {total} tests to run, {ignored} ignored, {sum(1 for t in whole.values() if t)} binaries")
    for what, n in rows[1:]:
        print(f"{what}: {n}")
    print("the buckets are disjoint and complete" if not problems else "\n".join(problems))
    summarize(f"Nextest hash buckets of {count}", rows, problems)
    return 1 if problems else 0


def parity(args):
    done = subprocess.run(
        ["cargo", "test", *args, "--", "--list"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8", errors="replace",
    )
    if done.returncode != 0:
        sys.exit(f"cargo test --list failed:\n{done.stdout[-2000:]}")
    cargo = read_cargo_list(done.stdout)
    listing = nextest(args)
    problems = compare_parity(cargo, listing)
    listed = names(listing)
    ignored = sum(1 for tests in listing.values() for flag, _ in tests.values() if flag)
    print(f"cargo test --list: {len(cargo)} tests in {len({b for b, _ in cargo})} binaries")
    print(f"nextest list: {len(listed)} tests ({ignored} ignored) in {len({b for b, _ in listed})} binaries")
    print("the same tests in every binary" if not problems else "\n".join(problems[:10]))
    summarize(
        "cargo test versus nextest",
        [("cargo test --list", len(cargo)), ("nextest list", len(listed)), ("of which ignored", ignored)],
        problems,
    )
    return 1 if problems else 0


def main(argv):
    if "--" not in argv:
        sys.exit(__doc__)
    split = argv.index("--")
    command, args = argv[:split], argv[split + 1:]
    if command[:1] == ["shards"] and len(command) == 2 and command[1].isdigit():
        return shards(int(command[1]), args)
    if command == ["parity"]:
        return parity(args)
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
