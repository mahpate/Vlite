"""Conservative CI routing and exact-source image publication checks."""
import hashlib
import json
import os
import re
import subprocess
import sys
import time


def docs_only(paths):
    return bool(paths) and all(
        path == "README.md"
        or (path.startswith("docs/") and path.endswith(".md"))
        for path in paths
    )


# The Docker job rebuilds the image from source and runs the whole suite inside it, which is
# the longest job of a run. It follows the files that can change its outcome: the image and
# compose definitions, the deployment assets, the container helper scripts, and the end-to-end
# scripts that only this job runs. Source changes are covered by the native matrix, and every
# release still runs the container suite (release.yml, image-test).
DOCKER_FILES = frozenset({
    "Dockerfile", ".dockerignore", "tests/docker_setup.py", ".github/workflows/ci.yml",
    "tests/rest_e2e.py", "tests/tls_e2e.py", "tests/dashboard_e2e.py", "tests/requirements-dashboard.txt",
})
DOCKER_PREFIXES = ("deploy/", "scripts/docker")
COMPOSE_FILE = re.compile(r"(docker-)?compose(\.[\w-]+)?\.ya?ml")


def touches_docker(paths):
    return any(
        path in DOCKER_FILES or path.startswith(DOCKER_PREFIXES) or COMPOSE_FILE.fullmatch(path)
        for path in paths
    )


def trusted_run(run, sha):
    return (
        run["head_sha"] == sha
        and run["head_branch"] == "main"
        and run["event"] in ("push", "workflow_dispatch")
        and run["conclusion"] == "success"
    )


NATIVE_OSES = ("ubuntu-latest", "ubuntu-24.04-arm", "windows-latest", "macos-latest")
TOOLCHAINS = ("1.89.0", "stable")
WINDOWS_SHARDS = 3


def all_succeeded(jobs, prefix, expected):
    """Exactly the expected job names carry the prefix, and every one of them succeeded."""
    found = [job for job in jobs if job["name"].startswith(prefix)]
    return (
        len(found) == len(expected)
        and {job["name"] for job in found} == expected
        and all(job["conclusion"] == "success" for job in found)
    )


def full_ci_jobs(jobs):
    native = {f"native ({os_name}, {rust})" for os_name in NATIVE_OSES for rust in TOOLCHAINS}
    windows = {
        f"windows-tests ({rust}, shard {shard}/{WINDOWS_SHARDS})"
        for rust in TOOLCHAINS
        for shard in range(1, WINDOWS_SHARDS + 1)
    }
    return (
        all_succeeded(jobs, "native (", native)
        and all_succeeded(jobs, "windows-tests (", windows)
        and all(
            any(job["name"] == name and job["conclusion"] == "success" for job in jobs)
            for name in ("lint", "docker", "default-features")
        )
    )


def gh_pages(endpoint, field):
    pages = subprocess.check_output(
        ["gh", "api", "--paginate", "--slurp", endpoint], text=True
    )
    return [item for page in json.loads(pages) for item in page[field]]


def image_ci():
    if os.environ["GITHUB_REF"] != "refs/heads/main":
        raise RuntimeError("Deployment images must be dispatched on main.")
    repo, sha = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_SHA"]
    runs = gh_pages(
        f"repos/{repo}/actions/workflows/ci.yml/runs"
        f"?head_sha={sha}&branch=main&status=success&per_page=100",
        "workflow_runs",
    )
    for run in runs:
        if trusted_run(run, sha):
            jobs = gh_pages(
                f"repos/{repo}/actions/runs/{run['id']}/attempts/{run['run_attempt']}"
                "/jobs?per_page=100", "jobs"
            )
            if full_ci_jobs(jobs):
                print(f"Verified full CI for {sha}: {run['html_url']}")
                with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
                    summary.write(f"Qualified CI: {run['html_url']}\n\n")
                return
    raise RuntimeError(
        "No successful full main CI run for this exact SHA. Wait for CI, or "
        "dispatch VLite CI on main (docs-only runs do not qualify), then retry. "
        "A run qualifies only if its Docker job succeeded: a push that changed no container "
        "file skips that job, so dispatch VLite CI on main with the Docker job enabled."
    )


def route():
    with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as source:
        event = json.load(source)
    kind = os.environ["GITHUB_EVENT_NAME"]
    # Unknown changes (a new branch, a manual run) take the whole matrix, Docker job included.
    full = docker = True
    if kind in ("push", "pull_request"):
        base = event["before"] if kind == "push" else event["pull_request"]["base"]["sha"]
        if base and set(base) != {"0"}:
            paths = subprocess.check_output(
                ["git", "diff", "--name-only", "-z", base, os.environ["GITHUB_SHA"]]
            ).decode("utf-8").rstrip("\0").split("\0")
            full = not docs_only(paths)
            docker = full and touches_docker(paths)
    elif os.environ.get("CI_DOCKER_INPUT", "").lower() == "false":
        # Only a manual run can leave the container suite out, for a quicker look at the rest.
        docker = False
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"full={str(full).lower()}\ndocker={str(docker).lower()}\n")
    if not full:
        print("Markdown-only documentation checks")
    elif docker:
        print("Full native/container matrix")
    else:
        print("Full native matrix; the Docker job is skipped (no container file changed)")


# Release CI guard: publication needs a successful full ci.yml run at the exact commit.
CI_PATH = ".github/workflows/ci.yml"
RELEASE_TAG = re.compile(r"v[0-9][0-9A-Za-z.+-]*")
COMMIT_SHA = re.compile(r"[0-9a-f]{40}")
SHA256_HEX = re.compile(r"[0-9a-f]{64}")
GH_TIMEOUT_SECONDS = 60
RETRY_BACKOFF_SECONDS = (5, 15)
POLL_SECONDS = 60


class GuardError(Exception):
    """The guard could not reach a verdict, so it fails closed."""


class GuardFailure(Exception):
    """Enforce mode found no qualifying run within its bounds."""


def gh_json(*args):
    """One `gh api` call with a hard timeout; a timeout raises like any other failure."""
    done = subprocess.run(
        ["gh", "api", *args], capture_output=True, text=True, timeout=GH_TIMEOUT_SECONDS
    )
    if done.returncode != 0:
        raise GuardError(f"gh api {args[-1]} exited {done.returncode}: {done.stderr.strip()}")
    return json.loads(done.stdout)


def gh_slurp(endpoint):
    return gh_json("--paginate", "--slurp", endpoint)


def gh_object(endpoint):
    return gh_json(endpoint)


def retried(call, sleep, clock, deadline):
    """At most three tries; each backoff is clamped so that no sleep passes `deadline`."""
    failure = None
    for backoff in RETRY_BACKOFF_SECONDS + (None,):
        try:
            return call()
        except Exception as error:  # any failure, a timeout included, is one failed try
            failure = error
        if backoff is not None:
            pause = min(backoff, max(0, deadline - clock()))
            if pause > 0:
                sleep(pause)
    raise GuardError(f"GitHub API failed after {len(RETRY_BACKOFF_SECONDS) + 1} tries: {failure!r}")


def slurped(pages, field, counted=False):
    if not (isinstance(pages, list) and pages and all(
        isinstance(page, dict) and isinstance(page.get(field), list) for page in pages
    )):
        raise GuardError(f"malformed {field} pages")
    items = [item for page in pages for item in page[field]]
    total = pages[0].get("total_count")
    if counted and (type(total) is not int or len(items) != total):
        raise GuardError(f"collected {len(items)} {field}, but total_count is {total!r}")
    return items


def wait_minutes(env, name, default, upper):
    value = env.get(name) or str(default)
    if not re.fullmatch(r"[0-9]{1,3}", value) or int(value) > upper:
        raise GuardError(f"{name} must be an integer from 0 to {upper}, not {value!r}")
    return int(value)


def target_sha(env, repo, api_object, retry):
    tag, ci_sha = env.get("CI_TAG") or "", env.get("CI_SHA") or ""
    if tag:
        if not RELEASE_TAG.fullmatch(tag):
            raise GuardError(f"CI_TAG {tag!r} is not a release tag")
        sha = retry(lambda: api_object(f"repos/{repo}/commits/refs/tags/{tag}")["sha"])
        if ci_sha and ci_sha != sha:
            raise GuardError(f"CI_SHA {ci_sha} is not {sha}, the commit of tag {tag}: the tag moved")
    else:
        sha = ci_sha or env.get("GITHUB_SHA", "")
    if not (isinstance(sha, str) and COMMIT_SHA.fullmatch(sha)):
        raise GuardError(f"the target commit {sha!r} is not 40 lowercase hex characters")
    return sha


def classify(run, repo, sha, workflow_id, jobs_of, refetch):
    """The qualification rules in order, on the latest attempt; None means not a candidate."""
    if run["workflow_id"] != workflow_id or run["path"] != CI_PATH or run["head_sha"] != sha:
        return None
    if run["event"] not in ("push", "workflow_dispatch"):
        return "rejected", f"event {run['event']}"
    head = run["head_repository"] or {}
    if not run["repository"]["full_name"] == head.get("full_name") == repo:
        return "rejected", "foreign repository"
    if run["status"] != "completed":
        return "pending", f"status {run['status']}"
    if run["conclusion"] != "success":
        return "rejected", f"conclusion {run['conclusion'] or 'null'}"
    jobs = jobs_of(run)
    required = [job for job in jobs if job["name"] == "CI required"]
    if not (len(required) == 1 and required[0]["conclusion"] == "success" and full_ci_jobs(jobs)):
        return "rejected", "partial CI (docs-only or Docker skipped)"
    fresh = refetch(run)
    if (fresh["run_attempt"], fresh["status"], fresh["conclusion"]) != (run["run_attempt"], "completed", "success"):
        return "pending", f"rerun started (attempt {fresh['run_attempt']}, status {fresh['status']})"
    return "qualified", "qualified"


def evaluate(repo, sha, workflow_id, api, api_object, retry):
    runs = retry(lambda: slurped(
        api(f"repos/{repo}/actions/workflows/ci.yml/runs?head_sha={sha}&per_page=100"),
        "workflow_runs", counted=True,
    ))

    def jobs_of(run):
        endpoint = f"repos/{repo}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100"
        return retry(lambda: slurped(api(endpoint), "jobs", counted=True))

    def refetch(run):
        return retry(lambda: api_object(f"repos/{repo}/actions/runs/{run['id']}"))

    candidates = []
    for run in runs:
        result = classify(run, repo, sha, workflow_id, jobs_of, refetch)
        if result:
            candidates.append((run, *result))
            if result[0] == "qualified":
                return "qualified", candidates
    states = {state for _, state, _ in candidates}
    verdict = "pending" if "pending" in states else "rejected" if candidates else "missing"
    return verdict, candidates


def candidate_lines(candidates):
    return "".join(
        f"- {run.get('html_url')}, {run.get('event')}, {run.get('head_branch')}, "
        f"attempt {run.get('run_attempt')}, {reason}\n"
        for run, _, reason in candidates
    ) or "- no candidate ci.yml run at this commit\n"


def remedy(env):
    ref = env.get("CI_TAG") or env.get("GITHUB_REF_NAME") or "<ref>"
    return (
        f"Remedy: run `gh workflow run ci.yml --ref {ref} -f docker=true` (the release branch or tag at "
        "this commit), or re-run a failed or partial CI run with Docker on. When it succeeds, use "
        "Re-run failed jobs on this run. No re-tag is needed.\n"
    )


def manifest_sources(directory, tag, sha):
    """The checksum-verified release manifest's sha256 and the commits its wheels were built from."""
    folder = os.path.join(directory, "")
    try:
        sums = {}
        with open(folder + "SHA256SUMS", encoding="ascii") as source:
            for line in source.read().splitlines():
                match = re.fullmatch(r"([0-9a-f]{64}) [ *]([^/\\]+)", line)
                if not match or match[2] in sums:
                    raise GuardError(f"malformed SHA256SUMS line {line!r}")
                sums[match[2]] = match[1]
        with open(folder + "release-manifest.json", "rb") as source:
            data = source.read()
        manifest = json.loads(data)
    except (OSError, ValueError) as error:
        raise GuardError(f"cannot read the release manifest: {error!r}") from error
    digest = hashlib.sha256(data).hexdigest()
    if sums.get("release-manifest.json") != digest:
        raise GuardError("release-manifest.json does not match SHA256SUMS")
    if not (isinstance(manifest, dict) and isinstance(manifest.get("assets"), list)):
        raise GuardError("malformed release manifest")
    if manifest.get("version") != tag.removeprefix("v"):
        raise GuardError(f"manifest version {manifest.get('version')!r} does not match tag {tag}")
    if manifest.get("source_commit") != sha:
        raise GuardError(f"manifest source_commit {manifest.get('source_commit')!r} is not {sha}, "
                         f"the commit of tag {tag}")
    names, sources = set(), set()
    for asset in manifest["assets"]:
        if not isinstance(asset, dict):
            raise GuardError(f"malformed manifest asset {asset!r}")
        name, source, hashed = asset.get("name"), asset.get("source_commit", sha), asset.get("sha256")
        # Membership first: an asset absent from SHA256SUMS with no hash must not match "no entry".
        if not (isinstance(name, str) and name not in names and name in sums
                and isinstance(hashed, str) and SHA256_HEX.fullmatch(hashed) and sums[name] == hashed
                and isinstance(source, str) and COMMIT_SHA.fullmatch(source)):
            raise GuardError(f"manifest asset {asset!r} is malformed, repeated or not in SHA256SUMS")
        names.add(name)
        if name.endswith(".whl"):
            sources.add(source)
    if not sources:
        raise GuardError("the release manifest lists no wheels")
    return [sha] + sorted(sources - {sha}), digest


def await_ci(env, mode, repo, sha, workflow_id, api, api_object, sleep, clock, start, wait, grace):
    """Polls one commit; returns its summary, or raises GuardFailure (enforce) when it never qualifies."""
    # Before the first verdict the active deadline is the guard's overall bound (grace <= wait);
    # afterwards it is the bound of the previous verdict. No poll or retry sleep passes it.
    deadline = start + 60 * max(wait, grace)
    while True:
        verdict, candidates = evaluate(
            repo, sha, workflow_id, api, api_object,
            lambda call, until=deadline: retried(call, sleep, clock, until),
        )
        if mode == "report":
            return (
                f"Release CI guard verdict: {verdict} (report mode; publication is never authorized) "
                f"for {sha}\n\n{candidate_lines(candidates)}"
                + ("" if verdict == "qualified" else "\n" + remedy(env))
            )
        if verdict == "qualified":
            return f"Release CI guard verdict: qualified for {sha}: {candidates[-1][0]['html_url']}\n"
        deadline = start + 60 * (wait if verdict == "pending" else grace)
        remaining = deadline - clock()
        if remaining <= 0:
            raise GuardFailure(
                f"Release CI guard verdict: {verdict}: no qualifying full CI run for {sha} within "
                f"{(wait if verdict == 'pending' else grace)} minutes.\n\n{candidate_lines(candidates)}\n"
                + remedy(env)
            )
        sleep(min(POLL_SECONDS, remaining))


def guard(env, mode, api, api_object, sleep, clock):
    """Returns (verified, summary, outputs) or raises GuardFailure (enforce) or another error.

    With CI_MANIFEST_DIR (PyPI), every distinct wheel source commit of the release manifest must
    qualify, not only the tag's commit."""
    start = clock()
    if mode not in ("enforce", "report"):
        raise GuardError(f"CI_GUARD_MODE must be enforce or report, not {mode!r}")
    if mode == "report" and env.get("GITHUB_REF", "").startswith("refs/tags/"):
        raise GuardError("a tag run cannot be report-only")
    wait = wait_minutes(env, "CI_WAIT_MINUTES", 120, 240)
    grace = wait_minutes(env, "CI_MISSING_GRACE_MINUTES", 15, wait)
    if env.get("CI_MANIFEST_DIR") and not env.get("CI_TAG"):
        raise GuardError("CI_MANIFEST_DIR needs CI_TAG")
    repo = env["GITHUB_REPOSITORY"]

    def retry(call):
        return retried(call, sleep, clock, start + 60 * max(wait, grace))
    sha = target_sha(env, repo, api_object, retry)
    shas, outputs = [sha], {}
    if env.get("CI_MANIFEST_DIR"):
        shas, outputs["manifest_sha256"] = manifest_sources(env["CI_MANIFEST_DIR"], env["CI_TAG"], sha)
    workflow_id = retry(lambda: api_object(f"repos/{repo}/actions/workflows/ci.yml")["id"])
    summary = "".join(
        await_ci(env, mode, repo, commit, workflow_id, api, api_object, sleep, clock, start, wait, grace)
        for commit in shas
    )
    return mode == "enforce", summary, outputs


def append(env, name, text):
    with open(env[name], "a", encoding="utf-8") as target:
        target.write(text)


def release_ci(env, api, api_object, sleep, clock):
    """Exit code of the guard; `verified=true` is written only by enforce mode after a pass."""
    mode = env.get("CI_GUARD_MODE") or "enforce"
    try:
        verified, summary, outputs = guard(env, mode, api, api_object, sleep, clock)
        verified = verified and mode == "enforce"
        print(summary)
        append(env, "GITHUB_STEP_SUMMARY", summary + "\n")
        append(env, "GITHUB_OUTPUT", f"verified={str(verified).lower()}\n" + "".join(
            f"{name}={value}\n" for name, value in outputs.items() if verified))
        return 0
    except Exception as error:
        text = str(error) if isinstance(error, GuardFailure) else f"Release CI guard verdict: error\n\n{error!r}\n"
        print(text, file=sys.stderr)
        for name, content in (("GITHUB_STEP_SUMMARY", text + "\n"), ("GITHUB_OUTPUT", "verified=false\n")):
            try:
                append(env, name, content)
            except Exception as failure:
                print(f"cannot write {name}: {failure!r}", file=sys.stderr)
        return 1


def release_ci_main():
    sys.exit(release_ci(os.environ, gh_slurp, gh_object, time.sleep, time.monotonic))


if __name__ == "__main__":
    {"route": route, "image-ci": image_ci, "release-ci": release_ci_main}[sys.argv[1]]()
